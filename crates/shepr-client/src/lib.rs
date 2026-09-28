//! Thin client mode - connects to the server's client socket.
//!
//! The client:
//! - Connects to `shepr-client.sock`, checks the build preamble, then sends terminal geometry
//! - Sets up the real terminal (raw mode, mouse capture, keyboard enhancements)
//! - Receives Frame messages and blits them to the terminal (diff against last frame)
//! - Reads stdin events (keystrokes, mouse, paste) and sends them as ClientMessage::Input
//! - Detects terminal resize and sends ClientMessage::Resize
//! - Restores terminal on exit (normal or error)
//! - Handles ServerShutdown gracefully (clean exit, informative message to stderr)
//! - Handles server unreachable (clear error screen, not blank/hang)
//! - Forwards OSC 52 clipboard writes from server to its own stdout

mod attach;
mod clipboard_forwarding;
pub mod endpoint;
mod errors;
mod events;
mod frame_output;
mod handshake;
pub(crate) mod host_replies;
mod input;
pub(crate) mod input_wire;
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

#[cfg(test)]
use clipboard_forwarding::decode_clipboard_payload;
use clipboard_forwarding::forward_clipboard;
use events::{ClientLoopEvent, ParsedHostInput};
use loop_config::{ClientLoopConfig, ClientSettings};
use shell_runtime::*;
use state::{AttachSession, ClientState, SessionMode, ShellSession};
use transport::*;

pub use shell::{ClientShellConfig, ClientShellState};
pub use startup::{run_client, run_terminal_attach};

use terminal_geometry::query_host_terminal_appearance;
use terminal_geometry::{AtomicCellSize, reported_cell_size_from_events, store_reported_cell_size};
#[cfg(test)]
use terminal_geometry::{
    cell_size_fallback, current_terminal_geometry_with, ioctl_cell_size, pack_cell_size,
    resize_report_required, write_host_cell_size_query, write_host_terminal_appearance_query,
    write_host_terminal_theme_query,
};
use terminal_geometry::{
    host_cell_size_query_required, initial_terminal_geometry, query_host_cell_size,
    query_host_terminal_theme, resize_poll_loop,
};
use terminal_setup::{
    HostMouseMode, TerminalGuard, setup_direct_attach_terminal, setup_terminal,
    should_draw_host_cursor,
};

#[cfg(test)]
use terminal_setup::{
    HostModes, effective_mouse_capture, effective_sgr_pixel_mouse,
    should_enable_host_color_scheme_reports, write_host_color_scheme_report_mode,
    write_terminal_restore_postlude,
};

use attach::AttachEscapeState;
use attach::direct_attach_pixel_mouse;
use attach::{AttachInputAction, attach_semantic_message};
pub use errors::ClientError;
use errors::ClientErrorContext;
#[cfg(test)]
use handshake::REMOTE_HANDSHAKE_READ_TIMEOUT;
use handshake::{ClientProcessRole, do_handshake};

use std::collections::VecDeque;
use std::io::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use interprocess::TryClone as _;
use interprocess::local_socket::traits::Stream as _;
use tracing::{debug, info, warn};

use shepr_platform::ipc::LocalStream;
use shepr_protocol::{ClientMessage, MAX_FRAME_SIZE, ServerMessage};
use shepr_termio::blit as render_ansi;

fn remember_direct_notice(notices: &mut VecDeque<String>, message: String) {
    const MAX_NOTICES: usize = 64;
    if notices.len() == MAX_NOTICES {
        let _ = notices.pop_front();
    }
    notices.push_back(message);
}

/// The reattach command the remote bridge hands the client it spawns, read
/// once at launch; unset for a local client.
fn reattach_command_from_env() -> io::Result<Option<String>> {
    shepr_core::env::read_text(shepr_core::env::EnvVar::SheprReattachCommand)
        .map_err(io::Error::from)
}

enum ClientLaunchMode {
    Shell,
    Attach {
        terminal_id: shepr_protocol::TerminalId,
        takeover: bool,
        escape: AttachEscapeState,
    },
}

fn run_client_with_mode(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
    mode: ClientLaunchMode,
    log_message: &'static str,
) -> io::Result<()> {
    let (attach_request, attach_escape) = match mode {
        ClientLaunchMode::Shell => (None, None),
        ClientLaunchMode::Attach {
            terminal_id,
            takeover,
            escape,
        } => (Some((terminal_id, takeover)), Some(escape)),
    };
    shepr_platform::logging::init_file_logging(
        &shepr_api::session::data_dir(paths),
        shepr_platform::logging::CLIENT_LOG_FILE,
    )?;

    let client_rendered_shell = attach_request.is_none();
    let socket_path = paths.server_address().client_socket().to_path_buf();
    let error_context = ClientErrorContext::new(
        paths.server_address().attach_command(paths.session_id()),
        reattach_command_from_env()?,
    );
    let role = ClientProcessRole::from_env().map_err(io::Error::other)?;
    let keybinding_source = role.keybinding_source();
    let shell_config = client_rendered_shell.then(|| {
        shell::ClientShellConfig::from_validated_config(config)
            .with_keybinding_source(keybinding_source)
            .with_local_endpoint(paths.state_dir(), &socket_path)
    });
    let mut settings = ClientSettings::from_config(config);
    let mouse_capture = settings.mouse_capture_active;
    let pixel_geometry_fallback = client_rendered_shell;
    let pixel_geometry_enabled = pixel_geometry_fallback || attach_escape.is_some();
    settings.pixel_geometry_enabled = pixel_geometry_enabled;
    settings.pixel_geometry_fallback = pixel_geometry_fallback;
    let mut loop_config = ClientLoopConfig {
        role,
        settings,
        host_escape_disambiguation_active: false,
        initial_host_input: Vec::new(),
        paths: paths.clone(),
        local_socket_path: socket_path.clone(),
        shell_config,
    };

    shepr_platform::logging::startup("client");
    info!(path = %socket_path.display(), "{log_message}");

    let endpoint_catalog = if client_rendered_shell && role == ClientProcessRole::Local {
        endpoint::EndpointCatalog::load(paths).map_err(|error| {
            io::Error::other(format!(
                "saved SSH endpoint catalog is unavailable: {error}"
            ))
        })?
    } else {
        endpoint::EndpointCatalog::default()
    };
    let local_failure_policy = endpoint::LocalFailurePolicy::for_catalog(&endpoint_catalog);

    let initial_stream = match shepr_platform::ipc::connect_local_stream(&socket_path) {
        Ok(stream) => Some(stream),
        Err(error) if !local_failure_policy.ends_client_for(&endpoint::ClientEndpointId::Local) => {
            warn!(%error, "Local is unavailable; keeping saved machines available");
            None
        }
        Err(error) => {
            return Err(io::Error::other(
                ClientError::ConnectionFailed(error).to_string(),
            ));
        }
    };

    // Get the terminal geometry before handshake (before raw mode).
    let geometry = initial_terminal_geometry(pixel_geometry_enabled, pixel_geometry_fallback)?;
    let (cols, rows) = (geometry.cols(), geometry.rows());

    let shell_surface_size = loop_config.shell_config.as_ref().map(|shell| {
        let host_size = terminal_geometry::ClientHostSize::new(cols, rows, true);
        shell.initial_surface_size(host_size.cols, host_size.rows)
    });
    // Healthy Local attaches directly; only an actual failure enters background recovery.
    let initial = initial_stream
        .map(|mut stream| {
            do_handshake(
                &mut stream,
                role,
                geometry,
                shell_surface_size,
                loop_config.settings.mouse_capture_active,
                true,
                None,
            )
            .map_err(|error| io::Error::other(error.to_string()))?;
            if let Some((terminal_id, takeover)) = attach_request {
                write_to_server(
                    &mut stream,
                    &ClientMessage::AttachTerminal {
                        terminal_id,
                        takeover,
                    },
                )?;
            }
            Ok(stream)
        })
        .transpose();
    let initial = match initial {
        Ok(initial) => initial,
        Err(error) if !local_failure_policy.ends_client_for(&endpoint::ClientEndpointId::Local) => {
            warn!(%error, "Local handshake failed; keeping saved machines available");
            None
        }
        Err(error) => return Err(error),
    };

    // A shell with saved machines can show connection notices without a server snapshot.
    let direct_attach = attach_escape.is_some();
    let mut terminal_guard = if direct_attach {
        setup_direct_attach_terminal(mouse_capture)
    } else {
        setup_terminal(mouse_capture)
    }
    .map_err(|err| {
        eprintln!("shepr: failed to set up terminal: {err}");
        err
    })?;
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
        warn!(%err, "failed to install termination handler; terminal restore relies on TerminalGuard::Drop and the panic hook");
    }

    let mut direct_notices = VecDeque::new();
    let result = rt.block_on(async {
        run_client_loop(
            initial,
            endpoint_catalog,
            local_failure_policy,
            geometry,
            should_quit,
            loop_config,
            attach_escape,
            &mut direct_notices,
            &terminal_guard,
        )
        .await
    });

    // Restore the terminal before printing any final status message.
    let terminal_restore_failed = terminal_guard.restore().is_err();
    // A later successful detach does not erase notices collected while forwarding earlier input.
    for notice in direct_notices {
        let _ = writeln!(io::stderr(), "shepr: {notice}");
    }

    if let Err(err) = result {
        let detached = matches!(
            &err,
            ClientError::ServerShutdown {
                reason: Some(reason)
            } if *reason == shepr_protocol::ShutdownReason::Detached
        );
        let error_message = err.display_with_context(&error_context);
        let _ = writeln!(io::stderr(), "shepr: {error_message}");
        rt.shutdown_timeout(Duration::from_millis(100));
        shepr_remote::release_ssh_resources_before_exit(Duration::from_secs(1));
        shepr_platform::logging::shutdown("client");

        let connection_lost_during_terminal_hangup =
            terminal_restore_failed && matches!(&err, ClientError::ConnectionLost(_));
        if detached || connection_lost_during_terminal_hangup {
            return Ok(());
        }

        std::process::exit(1);
    }

    rt.shutdown_timeout(Duration::from_millis(100));
    shepr_remote::release_ssh_resources_before_exit(Duration::from_secs(1));
    shepr_platform::logging::shutdown("client");
    Ok(())
}

/// The main client event loop.
///
/// Uses a threaded architecture:
/// - stdin reader thread → sends parsed input events with raw bytes retained for attach
/// - resize poller thread → sends resize events to main loop
/// - server reader thread → reads ServerMessages and sends to main loop
/// - main loop: coordinates input, output, and server communication
// The startup handshake consumes these launch values once before building ClientLoop.
#[allow(clippy::too_many_arguments)]
async fn run_client_loop(
    initial: Option<LocalStream>,
    endpoint_catalog: endpoint::EndpointCatalog,
    local_failure_policy: endpoint::LocalFailurePolicy,
    initial_geometry: shepr_core::geometry::HostGeometry,
    should_quit: Arc<AtomicBool>,
    mut config: ClientLoopConfig,
    attach_escape: Option<AttachEscapeState>,
    direct_notices: &mut VecDeque<String>,
    terminal_guard: &TerminalGuard,
) -> Result<(), ClientError> {
    let (cols, rows) = (initial_geometry.cols(), initial_geometry.rows());
    let (initial_cell_width_px, initial_cell_height_px, initial_pixel_geometry_exact) = (
        initial_geometry.cell_width(),
        initial_geometry.cell_height(),
        initial_geometry.exact,
    );
    let draw_host_cursor =
        attach_escape.is_none() && should_draw_host_cursor(config.settings.host_cursor);
    let local_unavailable = initial.is_none();
    let (initial_cell_width_px, initial_cell_height_px, initial_pixel_geometry_exact) =
        terminal_geometry::bounded_cell_geometry(
            initial_cell_width_px,
            initial_cell_height_px,
            initial_pixel_geometry_exact,
        );

    let host_modes = terminal_guard.host_modes();
    host_modes.configure_mouse_mode(HostMouseMode::new(
        attach_escape.is_some() && config.settings.mouse_capture_active,
        config.settings.mouse_capture_active,
        config.settings.mouse_capture_active,
    ));
    let mut state = ClientState {
        blit_encoder: render_ansi::BlitEncoder::new(),
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
        mode: match config.shell_config.take().map(shell::ClientShellState::new) {
            Some(shell) => SessionMode::Shell(Box::new(shell)),
            None => SessionMode::DirectAttach(AttachSession {
                escape: attach_escape,
            }),
        },
        repaint_pending: false,
        presentation_frozen: false,
        deferred_local_activation: None,
        draw_host_cursor,
    };
    state.set_host_size(cols, rows);
    // Only a client that loaded the saved machines follows them; attach and remote-client
    // processes run with an empty catalog.
    let catalog_watch = (state.mode.is_shell() && config.role == ClientProcessRole::Local)
        .then(|| endpoint::EndpointCatalogWatch::new(&config.paths, std::time::Instant::now()));
    let freeze_recovery_attempted = None;
    if let Some(shell) = state.mode.shell_mut() {
        shell.set_endpoint_catalog(&endpoint_catalog.ssh);
        if local_unavailable {
            shell.set_endpoint_status(
                &endpoint::ClientEndpointId::Local,
                endpoint::ClientEndpointStatus::Connecting,
            );
        }
    }
    // Cell size reported by the host terminal, packed as width<<32 | height.
    // Zero means the host has not reported one.
    let reported_cell_size = Arc::new(AtomicCellSize::new());
    let (stdin_mouse_capture_active, stdin_sgr_pixels_active) =
        state.host_modes.mouse_input_mirrors();

    // Channel for events from the resize and server reader threads.
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<ClientLoopEvent>(256);
    let (supervisor_tx, supervisor_rx) =
        tokio::sync::mpsc::channel::<endpoint::EndpointSupervisorEvent>(64);
    let stdin_tx = event_tx.clone();

    let endpoint_commands = endpoint::commands::EndpointCommands::default();

    // Spawn the stdin reader thread.
    let will_query_host_terminal_theme = !state.mode.is_escape_attach();
    // Terminals that report no pixel size through the ioctl are asked directly
    // instead of falling back to an assumed cell size.
    let will_query_host_cell_size = !state.mode.is_escape_attach()
        && host_cell_size_query_required(state.settings.pixel_geometry_enabled);
    let stdin_quit = Arc::clone(&should_quit);
    let stdin_escape_disambiguation_active = config.host_escape_disambiguation_active;
    let stdin_initial_host_input = std::mem::take(&mut config.initial_host_input);
    std::thread::spawn(move || {
        input::stdin_reader_loop(
            &stdin_tx,
            &stdin_quit,
            will_query_host_terminal_theme,
            will_query_host_cell_size,
            &stdin_mouse_capture_active,
            &stdin_sgr_pixels_active,
            stdin_escape_disambiguation_active,
            &stdin_initial_host_input,
        );
    });

    if will_query_host_terminal_theme {
        query_host_terminal_theme();
        if state.mode.is_shell() {
            query_host_terminal_appearance();
        }
    }

    if will_query_host_cell_size {
        query_host_cell_size();
    }

    // Spawn the resize poller thread.
    let resize_quit = Arc::clone(&should_quit);
    let resize_tx = event_tx.clone();
    let resize_cell_size = Arc::clone(&reported_cell_size);
    let pixel_geometry_enabled = state.settings.pixel_geometry_enabled;
    let pixel_geometry_fallback = config.settings.pixel_geometry_fallback;
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
            pixel_geometry_enabled,
            pixel_geometry_fallback,
            &resize_cell_size,
            &resize_quit,
        );
    });

    // A `--remote` child reaches its server through the SSH bridge, so its Local slot is
    // health-checked like a saved machine. Only the shell marks an endpoint ready (on its
    // first snapshot), so a direct attach keeps the plain socket rule.
    let local_link =
        if matches!(config.role, ClientProcessRole::Remote { .. }) && state.mode.is_shell() {
            endpoint::LocalEndpointLink::SshBridge
        } else {
            endpoint::LocalEndpointLink::Socket
        };
    let write_stream = if let Some(stream) = initial {
        let max_frame_size = shepr_protocol::MAX_FRAME_SIZE;
        let surface_decoder = shepr_protocol::surface_reuse::Decoder::default();
        let transport = start_endpoint_transport(
            stream,
            (),
            &event_tx,
            endpoint::ClientEndpointId::Local,
            1,
            max_frame_size,
            surface_decoder,
        )?;
        let mut registry = endpoint::EndpointRegistry::with_local_link(transport, 1, local_link);
        if state.mode.is_shell() {
            registry.send(&ClientMessage::ClientShellFocus { focused: true });
        }
        registry
    } else {
        endpoint::EndpointRegistry::empty(local_link)
    };
    let mut supervisors = endpoint::EndpointSupervisors::with_ssh_settings(
        &config.paths,
        &endpoint_catalog.ssh,
        shepr_remote::SavedSshSettings {
            manage_ssh_config: config.settings.manage_ssh_config,
        },
        std::time::Instant::now(),
    )
    .map_err(ClientError::EndpointSetup)?;
    if local_failure_policy.reconnects_local() {
        supervisors.add_local(
            config.paths.server_address().client_socket().to_path_buf(),
            write_stream
                .connection(&endpoint::ClientEndpointId::Local)
                .map(|connection| connection.generation.get()),
            std::time::Instant::now(),
        );
    }
    if local_unavailable
        && let Some(frame) = state.mode.shell_mut().and_then(|shell| {
            shell.compose(
                state.reported_geometry.cols(),
                state.reported_geometry.rows(),
            )
        })
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
        direct_notices,
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

struct ClientLoop<'a> {
    state: ClientState,
    endpoint_catalog: endpoint::EndpointCatalog,
    local_failure_policy: endpoint::LocalFailurePolicy,
    should_quit: Arc<AtomicBool>,
    config: ClientLoopConfig,
    direct_notices: &'a mut VecDeque<String>,
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

impl ClientLoop<'_> {
    async fn run(&mut self) -> Result<(), ClientError> {
        while !self.should_quit.load(Ordering::Acquire) {
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
            if let Some(shell) = self.state.mode.shell() {
                let cell = shepr_protocol::ProtocolCellSize::from_host(
                    self.state.reported_geometry.cell_width(),
                    self.state.reported_geometry.cell_height(),
                    self.state.reported_geometry.exact,
                );
                self.supervisors.spawn_due(
                    std::time::Instant::now(),
                    endpoint::EndpointConnectOptions {
                        geometry: shepr_core::geometry::HostGeometry::new(
                            self.state.reported_geometry.cols(),
                            self.state.reported_geometry.rows(),
                            cell.width(),
                            cell.height(),
                            cell.exact,
                        ),
                        surface_size: shell.surface_size(
                            self.state.reported_geometry.cols(),
                            self.state.reported_geometry.rows(),
                        ),
                        mouse_capture: self.state.host_modes.mouse_shell_preference(),
                    },
                    &self.supervisor_tx,
                );
            }
            let timer_delay = self
                .state
                .mode
                .shell()
                .map_or(Duration::from_millis(100), |shell| {
                    shell.timer_delay(std::time::Instant::now())
                });
            let timer_deadline = self
                .client_timer
                .deadline(std::time::Instant::now(), timer_delay);
            let event = if let Some(event) = self.scheduled_activation.take() {
                event
            } else {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(timer_deadline.into()) => ClientLoopEvent::Timer,
                    ev = self.supervisor_rx.recv() => ev.map(ClientLoopEvent::EndpointSupervisor).unwrap_or(ClientLoopEvent::Timer),
                    ev = self.event_rx.recv() => ev.unwrap_or(ClientLoopEvent::Timer),
                }
            };
            let now = std::time::Instant::now();
            if self.handle_event(event, now)? == ClientLoopAction::Exit {
                return Ok(());
            }
        }

        // Clean exit (Ctrl+C). Send Detach before closing.
        let _ = write_to_server(&mut self.write_stream, &ClientMessage::Detach);
        let _ = io::stdout().flush();
        Ok(())
    }

    fn handle_event(
        &mut self,
        event: ClientLoopEvent,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, ClientError> {
        match event {
            ClientLoopEvent::StdinInput(inputs) => self.handle_stdin_input(inputs),
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
    ) -> Result<ClientLoopAction, ClientError> {
        let Self {
            state,
            write_stream,
            pending_activation,
            endpoint_commands,
            scheduled_activation,
            direct_notices,
            reported_cell_size,
            will_query_host_cell_size,
            ..
        } = self;
        let shell_mode = state.mode.is_shell();
        if shell_mode {
            let raw_events = inputs.iter().map(|input| &input.event);
            if *will_query_host_cell_size
                && let Some((width_px, height_px)) = reported_cell_size_from_events(raw_events)
            {
                store_reported_cell_size(reported_cell_size, width_px, height_px);
            }
            if shepr_termio::input::raw_input::events_require_host_mode_refresh(
                inputs.iter().map(|input| &input.event),
            ) && let Err(err) =
                state
                    .host_modes
                    .apply_mouse(shell_mode, state.reported_geometry.exact, true)
            {
                warn!(err = %err, "failed to re-assert host mouse capture");
            }
            let host_reports_all_keys = state.host_modes.keyboard_report_all_active();
            let Some(shell) = state.mode.shell_mut() else {
                return Ok(ClientLoopAction::NextEvent);
            };
            let outcome = shell.handle_host_input(inputs, host_reports_all_keys);
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
            )? {
                return Ok(ClientLoopAction::Exit);
            }
            return Ok(ClientLoopAction::NextEvent);
        }
        if let Some(attach_escape) = state.mode.attach_escape_mut() {
            // Palette replies are deliberately grouped by the reader. They cannot
            // contain attach keys or mouse events, so retain that transport batch.
            if inputs.len() > 1 {
                let mut data = attach_escape.take_pending_prefix().unwrap_or_default();
                data.extend(inputs.into_iter().flat_map(|input| input.raw));
                if let Some(notice) = attach::forward_input(write_stream, &data).notice() {
                    remember_direct_notice(direct_notices, notice);
                }
                return Ok(ClientLoopAction::NextEvent);
            }
            let Some(input) = inputs.into_iter().next() else {
                return Ok(ClientLoopAction::NextEvent);
            };
            if let Some(pixels) = input.pixel_mouse {
                if let Some(prefix) = attach_escape.take_pending_prefix()
                    && let Some(notice) = attach::forward_input(write_stream, &prefix).notice()
                {
                    remember_direct_notice(direct_notices, notice);
                }
                if let Some((kind, position, modifiers)) =
                    direct_attach_pixel_mouse(&input.event, pixels)
                {
                    let geometry = pixels.geometry;
                    let message = ClientMessage::AttachMouse {
                        kind,
                        position,
                        geometry: Some(shepr_protocol::ClientMouseGeometry {
                            cols: geometry.cols(),
                            rows: geometry.rows(),
                            width_px: geometry.width_px,
                            height_px: geometry.height_px,
                        }),
                        modifiers: shepr_protocol::WireModifiers::from_bits_retain(modifiers),
                        lines: state.settings.mouse_scroll_lines,
                    };
                    write_stream.send(&message);
                }
                return Ok(ClientLoopAction::NextEvent);
            }
            let action = attach_escape.filter_parsed_input(
                input.raw,
                &input.event,
                state.reported_geometry.rows(),
                state.settings.mouse_scroll_lines,
            );
            match action {
                AttachInputAction::Forward(data) => {
                    if let Some(notice) = attach::forward_input(write_stream, &data).notice() {
                        remember_direct_notice(direct_notices, notice);
                    }
                }
                // Registry sends cannot fail here: a failed write is recorded against
                // its endpoint; the timer applies LocalFailurePolicy if Local broke.
                AttachInputAction::ForwardPair(first, second) => {
                    for data in [first, second] {
                        if let Some(notice) = attach::forward_input(write_stream, &data).notice() {
                            remember_direct_notice(direct_notices, notice);
                        }
                    }
                }
                AttachInputAction::Semantic(action) => {
                    if let Some(message) = attach_semantic_message(action) {
                        write_stream.send(&message);
                    }
                }
                AttachInputAction::ForwardThenSemantic(prefix, action) => {
                    if let Some(notice) = attach::forward_input(write_stream, &prefix).notice() {
                        remember_direct_notice(direct_notices, notice);
                    }
                    if let Some(message) = attach_semantic_message(action) {
                        write_stream.send(&message);
                    }
                }
                AttachInputAction::Detach => {
                    let _ = write_to_server(write_stream, &ClientMessage::Detach);
                    return Ok(ClientLoopAction::Exit);
                }
                AttachInputAction::ForwardThenDetach(data) => {
                    if let Some(notice) = attach::forward_input(write_stream, &data).notice() {
                        remember_direct_notice(direct_notices, notice);
                    }
                    let _ = write_to_server(write_stream, &ClientMessage::Detach);
                    return Ok(ClientLoopAction::Exit);
                }
                AttachInputAction::None => {}
            }
            return Ok(ClientLoopAction::NextEvent);
        }

        if inputs.iter().any(|input| input.pixel_mouse.is_some()) {
            return Ok(ClientLoopAction::NextEvent);
        }
        if shepr_termio::input::raw_input::events_require_host_surface_redraw(
            inputs.iter().map(|input| &input.event),
            state.settings.redraw_on_focus_gained,
        ) {
            state.request_repaint();
        }
        if shepr_termio::input::raw_input::events_require_host_terminal_appearance_query(
            inputs.iter().map(|input| &input.event),
        ) {
            query_host_terminal_appearance();
        }
        if shepr_termio::input::raw_input::events_require_host_terminal_theme_query(
            inputs.iter().map(|input| &input.event),
        ) {
            query_host_terminal_theme();
        }
        if let Some((width_px, height_px)) =
            reported_cell_size_from_events(inputs.iter().map(|input| &input.event))
        {
            store_reported_cell_size(reported_cell_size, width_px, height_px);
        }
        let data = inputs
            .into_iter()
            .flat_map(|input| input.raw)
            .collect::<Vec<_>>();
        if let Some(notice) = attach::forward_input(write_stream, &data).notice() {
            remember_direct_notice(direct_notices, notice);
        }
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_terminal_unavailable(
        &mut self,
        err: &io::Error,
    ) -> Result<ClientLoopAction, ClientError> {
        let Self { write_stream, .. } = self;
        info!(err = %err, "client terminal unavailable; detaching");
        let _ = write_to_server(write_stream, &ClientMessage::Detach);
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
            .apply_mouse(state.mode.is_shell(), pixel_geometry_exact, false)
            .map_err(ClientError::HostTerminal)?;
        state.set_host_size(new_cols, new_rows);
        // Resizing invalidates the host-side blit baseline. The retained pane surface
        // stays: until the resized one arrives, `compose` draws it clipped to the new
        // pane area (with pane hits clipped to match) instead of dropping to the
        // machine-list placeholder.
        state.request_repaint();
        let msg = if let Some(shell) = state.mode.shell() {
            client_shell_resize_message(
                shell,
                state.reported_geometry.cols(),
                state.reported_geometry.rows(),
                cell_width_px,
                cell_height_px,
                pixel_geometry_exact,
            )
        } else {
            ClientMessage::Resize {
                geometry: shepr_protocol::TerminalGeometry::new(
                    new_cols,
                    new_rows,
                    cell_width_px,
                    cell_height_px,
                    pixel_geometry_exact,
                ),
            }
        };
        if let Some(activation) = pending_activation.as_mut() {
            if let Err(error) = activation.update_resize(&msg, write_stream) {
                rollback_endpoint_activation(
                    state,
                    write_stream,
                    pending_activation,
                    &error,
                    false,
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
        if let Some(frame) = state.mode.shell_mut().and_then(|shell| {
            shell.compose(
                state.reported_geometry.cols(),
                state.reported_geometry.rows(),
            )
        }) {
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
            } => {
                if !supervisors.record_status(&endpoint_id, generation, status, now) {
                    return Ok(ClientLoopAction::NextEvent);
                }
                if status == endpoint::ClientEndpointStatus::Attention {
                    warn!(endpoint = %endpoint_id.storage_key(), generation, error = %message, "endpoint needs attention");
                }
                let unavailable = state.mode.shell_mut().and_then(|shell| {
                    shell.set_endpoint_status(&endpoint_id, status);
                    shell.set_machine_diagnostic(&endpoint_id, &message);
                    (status == endpoint::ClientEndpointStatus::Attention
                        && shell.endpoint_is_active(&endpoint_id))
                    .then(|| format!("{}: {message}", shell.endpoint_label(&endpoint_id)))
                });
                if let Some(message) = unavailable {
                    present_handoff_unavailable(state, message);
                } else if let Some(frame) = state.mode.shell_mut().and_then(|shell| {
                    shell.compose(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    )
                }) {
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
            } => {
                if !supervisors.record_status(
                    &endpoint_id,
                    generation,
                    endpoint::ClientEndpointStatus::Online,
                    now,
                ) {
                    return Ok(ClientLoopAction::NextEvent);
                }
                let frame = state.mode.shell_mut().and_then(|shell| {
                    shell.compose(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    )
                });
                let reader_quit = writer.stop_handle();
                write_stream.insert(endpoint_id.clone(), writer, generation, false);
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
                    MAX_FRAME_SIZE,
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
            direct_notices,
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
                        )?
                    {
                        *scheduled_activation = Some(event);
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                let composed = if let Some(shell) = state.mode.shell_mut() {
                    shell.set_pane_surface(surface);
                    shell.compose(
                        state.reported_geometry.cols(),
                        state.reported_geometry.rows(),
                    )
                } else {
                    None
                };
                if let Some(frame) = composed {
                    state.present_frame(frame);
                }
            }
            ServerMessage::PaneSurfacePatch(patch) => {
                let outcome = state
                    .mode
                    .shell_mut()
                    .map(|shell| shell.apply_pane_surface_patch(&patch));
                let compose_fallback = match outcome {
                    Some(shell::ClientPaneSurfacePatchOutcome::Applied(Some(patch))) => {
                        match state.present_surface_patch(patch) {
                            Ok(presented) => !presented,
                            Err(error) => {
                                warn!(%error, "failed to present retained pane surface patch");
                                state.request_repaint();
                                false
                            }
                        }
                    }
                    Some(shell::ClientPaneSurfacePatchOutcome::Applied(None)) => true,
                    Some(shell::ClientPaneSurfacePatchOutcome::Rejected) | None => false,
                };
                if compose_fallback {
                    let composed = state.mode.shell_mut().and_then(|shell| {
                        shell.compose(
                            state.reported_geometry.cols(),
                            state.reported_geometry.rows(),
                        )
                    });
                    if let Some(frame) = composed {
                        state.present_frame(frame);
                    }
                }
            }
            ServerMessage::Terminal(frame) => {
                let mut stdout = io::stdout();
                let _ = stdout.write_all(&frame.bytes);
                let _ = stdout.flush();
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
                if let Some(shell) = state.mode.shell_mut()
                    && shell.receive_endpoint_error(kind.to_string())
                    && let Some(frame) = shell.compose(
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
                        );
                        return Ok(ClientLoopAction::NextEvent);
                    }
                    let progress = pending_activation.as_mut().map(|pending| {
                        pending.receive_response_for_boot(
                            endpoint_id,
                            generation,
                            &boot_id,
                            &request_id,
                            &data,
                            write_stream,
                        )
                    });
                    match progress {
                        Some(endpoint::SurfaceActivationProgress::Ready) => {
                            if let Some(event) = complete_endpoint_activation(
                                state,
                                write_stream,
                                pending_activation,
                                endpoint_commands,
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
                let (outcome, frame) = match state.mode.shell_mut() {
                    Some(shell) => {
                        let outcome = if completed.generation == generation
                            && shell.endpoint_is_active(&completed.endpoint_id)
                        {
                            shell.handle_endpoint_result(
                                &completed.boot_id,
                                &completed.request_id,
                                completed.result,
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
                        (outcome, frame)
                    }
                    None => (shell::ClientShellInput::default(), None),
                };
                if finish_client_shell_input(
                    state,
                    outcome,
                    frame,
                    write_stream,
                    pending_activation,
                    endpoint_commands,
                    scheduled_activation,
                )? {
                    return Ok(ClientLoopAction::Exit);
                }
            }
            ServerMessage::DirectTerminalNotice { kind } => {
                if state.mode.is_escape_attach() {
                    remember_direct_notice(direct_notices, kind.to_string());
                }
            }
            ServerMessage::Clipboard { data } => {
                forward_clipboard(&data);
                let _ = io::stdout().flush();
            }
            ServerMessage::WindowTitle { title } => {
                // `None` is deliberate from the server (an API title was
                // cleared, or every template token resolved empty) and
                // resets to Shepr's default. A disabled `ui.window_title`
                // never reaches here: the server sends nothing at all.
                let _ = state
                    .host_modes
                    .write_window_title(&mut io::stdout(), title.as_deref());
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
                    .apply_mouse(state.mode.is_shell(), state.reported_geometry.exact, false)
                    .map_err(ClientError::HostTerminal)?;
            }
            ServerMessage::DirectTerminalKeyboardProtocol {
                flags,
                modify_other_keys_level,
            } => {
                if state.mode.is_escape_attach() {
                    state
                        .host_modes
                        .set_direct_keyboard_protocol(
                            &mut io::stdout(),
                            flags,
                            modify_other_keys_level,
                        )
                        .map_err(ClientError::HostTerminal)?;
                }
            }
            ServerMessage::ClientShellKeyboardReportAll { enabled } => {
                if state.mode.is_shell() {
                    let shell_requests_report_all = state
                        .mode
                        .shell()
                        .is_some_and(ShellSession::host_keyboard_report_all_requested);
                    state
                        .host_modes
                        .set_pane_keyboard_report_all(
                            &mut io::stdout(),
                            enabled,
                            shell_requests_report_all,
                        )
                        .map_err(ClientError::HostTerminal)?;
                }
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
                    )?
                {
                    *scheduled_activation = Some(event);
                }
                return Ok(ClientLoopAction::NextEvent);
            }
            ServerMessage::HealthPong(_) => return Ok(ClientLoopAction::NextEvent),
            ServerMessage::EndpointWelcome(_) => return Ok(ClientLoopAction::NextEvent),
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
                let activation_ready = state.mode.shell().is_some_and(|shell| {
                    shell.endpoint_has_snapshot(&selected_endpoint)
                        && (!write_stream
                            .connection(write_stream.active_id())
                            .is_some_and(|connection| connection.surface_active)
                            || shell.endpoint_boot_id(write_stream.active_id()).is_some())
                });
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
                debug!("received unexpected Welcome in main loop");
            }
            ServerMessage::SurfaceUpdate(_) => {
                return Err(ClientError::SurfaceUpdateBeforeDecode);
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
                clear_endpoint_host_effects(state);
            }
        }
        // A revoked transport changes the safe rollback destination. Handle those
        // failures before applying a timeout to the remaining activation phase.
        if let Some(endpoint_id) = pending_activation
            .as_ref()
            .filter(|activation| activation.expired(now))
            .map(|activation| activation.target().clone())
        {
            let label = state
                .mode
                .shell()
                .map(|shell| shell.endpoint_label(&endpoint_id).to_owned())
                .unwrap_or_else(|| "Endpoint".into());
            rollback_endpoint_activation(
                state,
                write_stream,
                pending_activation,
                &format!("{label} did not produce a coherent surface in time"),
                false,
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
                    clear_endpoint_host_effects(state);
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
        if state.mode.is_shell() {
            let expired_endpoints = endpoint_commands
                .expire(now)
                .into_iter()
                .filter(|expired| write_stream.accepts(&expired.endpoint_id, expired.generation))
                .collect::<Vec<_>>();
            let Some(shell) = state.mode.shell_mut() else {
                return Ok(ClientLoopAction::NextEvent);
            };
            let (outcome, frame) = {
                let mut outcome = shell.tick_selection_autoscroll(now);
                for expired in expired_endpoints {
                    if !shell.endpoint_is_active(&expired.endpoint_id) {
                        continue;
                    }
                    let expired_outcome = shell.handle_endpoint_result(
                        &expired.boot_id,
                        &expired.request_id,
                        expired.result,
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
            )? {
                return Ok(ClientLoopAction::Exit);
            }
        }
        Ok(ClientLoopAction::NextEvent)
    }
}

#[cfg(test)]
mod tests;
