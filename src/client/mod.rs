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
pub(crate) mod endpoint;
mod endpoint_commands;
mod endpoint_selection;
mod errors;
mod events;
mod frame_output;
mod handshake;
mod input;
mod loop_config;
mod shell;
mod shell_runtime;
mod startup;
mod state;
mod terminal_geometry;
mod terminal_setup;
mod timer;
mod transport;

#[cfg(test)]
use clipboard_forwarding::decode_clipboard_payload;
use clipboard_forwarding::forward_clipboard;
use events::ClientLoopEvent;
use loop_config::ClientLoopConfig;
use shell_runtime::*;
use state::ClientState;
use transport::*;

#[cfg(test)]
pub(crate) use shell::{ClientShellConfig, ClientShellState};
pub use startup::{run_client, run_terminal_attach};

use terminal_geometry::query_host_terminal_appearance;
#[cfg(test)]
use terminal_geometry::{
    cell_size_fallback, current_terminal_geometry_with, ioctl_cell_size, pack_cell_size,
    resize_report_required, should_query_host_cell_size, write_host_cell_size_query,
    write_host_terminal_appearance_query, write_host_terminal_theme_query,
};
use terminal_geometry::{
    host_cell_size_query_required, initial_terminal_geometry, query_host_cell_size,
    query_host_terminal_theme, resize_poll_loop, should_query_host_terminal_theme,
};
use terminal_geometry::{reported_cell_size_from_events, store_reported_cell_size};
use terminal_setup::{
    TerminalGuard, effective_mouse_capture, effective_sgr_pixel_mouse, host_mouse_capture_update,
    set_mouse_capture, setup_direct_attach_terminal, setup_terminal, should_draw_host_cursor,
};

fn refresh_host_mouse_capture(enabled: bool, sgr_pixels: bool) {
    if let Err(err) = set_mouse_capture(enabled, sgr_pixels) {
        warn!(err = %err, "failed to re-assert host mouse capture");
    }
}

#[cfg(test)]
use terminal_setup::{
    should_enable_host_color_scheme_reports, write_host_color_scheme_report_mode,
    write_terminal_restore_postlude,
};

use attach::AttachEscapeState;
use attach::direct_attach_pixel_mouse;
use attach::{AttachInputAction, attach_semantic_message};
pub use errors::ClientError;
#[cfg(test)]
use handshake::{REMOTE_HANDSHAKE_READ_TIMEOUT, handshake_read_timeout};
use handshake::{client_shell_keybinding_source, do_handshake, is_remote_client_process};

use std::collections::VecDeque;
use std::io::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use interprocess::TryClone as _;
use interprocess::local_socket::traits::Stream as _;
use tracing::{debug, info, warn};

use crate::ipc::LocalStream;
use crate::protocol::render_ansi;
use crate::protocol::{self, ClientMessage, MAX_FRAME_SIZE, ServerMessage};
use crate::server::socket_paths::client_socket_path;

fn remember_direct_notice(notices: &mut VecDeque<String>, message: String) {
    const MAX_NOTICES: usize = 64;
    if notices.len() == MAX_NOTICES {
        let _ = notices.pop_front();
    }
    notices.push_back(message);
}

fn run_client_with_mode(
    config: &crate::config::Config,
    paths: &crate::config::AppPaths,
    attach_request: Option<(String, bool)>,
    attach_escape: Option<AttachEscapeState>,
    log_message: &'static str,
) -> io::Result<()> {
    crate::logging::init_file_logging(paths, crate::logging::CLIENT_LOG_FILE);

    let attach_escape = attach_escape.map(|_| AttachEscapeState::from_config(config));
    crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout())?;
    let client_rendered_shell = attach_request.is_none();
    let socket_path = client_socket_path(paths);
    let keybinding_source = client_shell_keybinding_source().map_err(io::Error::other)?;
    let shell_config = client_rendered_shell.then(|| {
        shell::ClientShellConfig::from_config(config)
            .with_keybinding_source(keybinding_source)
            .with_local_endpoint(paths.state_dir(), &socket_path)
    });
    let mouse_capture = config.ui.mouse_capture;
    let mouse_scroll_lines = config.ui.mouse_scroll_lines();
    let redraw_on_focus_gained = config.ui.redraw_on_focus_gained;
    let host_cursor = config.ui.host_cursor;
    let pixel_geometry_fallback = client_rendered_shell;
    let pixel_geometry_enabled = pixel_geometry_fallback || attach_escape.is_some();
    let mut loop_config = ClientLoopConfig {
        mouse_scroll_lines,
        redraw_on_focus_gained,
        host_cursor,
        pixel_geometry_enabled,
        pixel_geometry_fallback,
        mouse_capture_active: mouse_capture,
        host_escape_disambiguation_active: false,
        initial_host_input: Vec::new(),
        manage_ssh_config: config.remote.manage_ssh_config,
        paths: paths.clone(),
        local_socket_path: socket_path.clone(),
        shell_config,
    };

    crate::logging::startup("client");
    info!(path = %socket_path.display(), "{log_message}");

    let endpoint_catalog = if client_rendered_shell && !is_remote_client_process() {
        endpoint::EndpointCatalog::load(paths).unwrap_or_else(|error| {
            warn!(%error, "saved SSH endpoint catalog is unavailable");
            endpoint::EndpointCatalog::default()
        })
    } else {
        endpoint::EndpointCatalog::default()
    };
    let federated = endpoint_catalog.has_ssh();

    let initial_stream = match crate::ipc::connect_local_stream(&socket_path) {
        Ok(stream) => Some(stream),
        Err(error) if federated => {
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
    let (cols, rows, cell_width_px, cell_height_px, exact_cell_size) =
        initial_terminal_geometry(pixel_geometry_enabled, pixel_geometry_fallback)?;

    let shell_surface_size = loop_config.shell_config.as_ref().map(|shell| {
        let bounded = protocol::ClientSurfaceSize { cols, rows }.clamped();
        shell.initial_surface_size(bounded.cols, bounded.rows)
    });
    // Healthy Local attaches directly; only an actual failure enters background recovery.
    let initial = initial_stream
        .map(|mut stream| {
            let handshake = do_handshake(
                &mut stream,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                exact_cell_size,
                shell_surface_size,
                loop_config.mouse_capture_active,
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
            Ok((stream, handshake))
        })
        .transpose();
    let initial = match initial {
        Ok(initial) => initial,
        Err(error) if federated => {
            warn!(%error, "Local handshake failed; keeping saved machines available");
            None
        }
        Err(error) => return Err(error),
    };

    // The federated shell can show connection notices without any server snapshot.
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
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            exact_cell_size,
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
            } if reason == "detached"
        );
        let error_message = err.display_with_target(paths.session_id(), paths.server_address());
        let _ = writeln!(io::stderr(), "shepr: {error_message}");
        rt.shutdown_timeout(Duration::from_millis(100));
        crate::remote::release_ssh_resources_before_exit(Duration::from_secs(1));
        crate::logging::shutdown("client");

        let connection_lost_during_terminal_hangup =
            terminal_restore_failed && matches!(&err, ClientError::ConnectionLost(_));
        if detached || connection_lost_during_terminal_hangup {
            return Ok(());
        }

        std::process::exit(1);
    }

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::remote::release_ssh_resources_before_exit(Duration::from_secs(1));
    crate::logging::shutdown("client");
    Ok(())
}

/// The main client event loop.
///
/// Uses a threaded architecture:
/// - stdin reader thread → sends raw input bytes to main loop
/// - resize poller thread → sends resize events to main loop
/// - server reader thread → reads ServerMessages and sends to main loop
/// - main loop: coordinates input, output, and server communication
// Each parameter is an independent piece of startup state threaded through
// from `main`; grouping them would just move the sprawl into an ad hoc
// struct without making the call sites clearer.
#[allow(clippy::too_many_arguments)]
async fn run_client_loop(
    initial: Option<(LocalStream, handshake::HandshakeResult)>,
    mut endpoint_catalog: endpoint::EndpointCatalog,
    cols: u16,
    rows: u16,
    initial_cell_width_px: u32,
    initial_cell_height_px: u32,
    initial_pixel_geometry_exact: bool,
    should_quit: Arc<AtomicBool>,
    mut config: ClientLoopConfig,
    attach_escape: Option<AttachEscapeState>,
    direct_notices: &mut VecDeque<String>,
    _terminal_guard: &TerminalGuard,
) -> Result<(), ClientError> {
    let draw_host_cursor = attach_escape.is_none() && should_draw_host_cursor(config.host_cursor);
    let local_unavailable = initial.is_none();
    let client_shell_size = config.shell_config.is_some();
    let displayed_size = if client_shell_size {
        let bounded = protocol::ClientSurfaceSize { cols, rows }.clamped();
        (bounded.cols, bounded.rows)
    } else {
        (cols, rows)
    };
    let (initial_cell_width_px, initial_cell_height_px, initial_pixel_geometry_exact) =
        terminal_geometry::bounded_cell_geometry(
            initial_cell_width_px,
            initial_cell_height_px,
            initial_pixel_geometry_exact,
        );

    let mut state = ClientState {
        blit_encoder: render_ansi::BlitEncoder::new(),
        mouse_capture_active: config.mouse_capture_active,
        endpoint_mouse_capture_requested: false,
        endpoint_sgr_pixels_requested: false,
        host_theme_updates: Vec::new(),
        direct_mouse_capture_preference: attach_escape.is_some() && config.mouse_capture_active,
        shell_mouse_capture_preference: config.mouse_capture_active,
        direct_keyboard_protocol: crate::terminal_modes::DirectHostKeyboardState::default(),
        pane_keyboard_report_all: false,
        keyboard_report_all_active: false,
        reported_size: displayed_size,
        reported_cell_size: (initial_cell_width_px, initial_cell_height_px),
        pixel_geometry_enabled: config.pixel_geometry_enabled,
        pixel_geometry_exact: initial_pixel_geometry_exact,
        attach_escape,
        mouse_scroll_lines: config.mouse_scroll_lines,
        redraw_on_focus_gained: config.redraw_on_focus_gained,
        repaint_pending: false,
        presentation_frozen: false,
        deferred_local_activation: None,
        draw_host_cursor,
        window_title_written: false,
        shell: config.shell_config.map(shell::ClientShellState::new),
    };
    // Whether this client keeps running without Local. It follows the live catalog: a client
    // that gains a saved machine survives losing Local from then on.
    let mut federated = endpoint_catalog.has_ssh();
    // Only a client that loaded the saved machines follows them; attach and remote-client
    // processes run with an empty catalog.
    let mut catalog_watch = (state.shell.is_some() && !is_remote_client_process())
        .then(|| endpoint::EndpointCatalogWatch::new(&config.paths, std::time::Instant::now()));
    let mut freeze_recovery_attempted = None;
    if let Some(shell) = state.shell.as_mut() {
        shell.set_endpoint_catalog(&endpoint_catalog.ssh);
        if local_unavailable {
            shell.set_endpoint_status(
                &endpoint::ClientEndpointId::Local,
                endpoint::ClientEndpointStatus::Connecting,
            );
        }
    }
    let host_mouse_capture_active = Arc::new(AtomicBool::new(state.mouse_capture_active));
    // Cell size reported by the host terminal, packed as width<<32 | height.
    // Zero means the host has not reported one.
    let reported_cell_size = Arc::new(AtomicU64::new(0));
    let host_sgr_pixels_active = Arc::new(AtomicBool::new(false));

    // Channel for events from the resize and server reader threads.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<ClientLoopEvent>(256);
    let (supervisor_tx, mut supervisor_rx) =
        tokio::sync::mpsc::channel::<endpoint::EndpointSupervisorEvent>(64);
    let stdin_tx = event_tx.clone();

    let mut endpoint_commands = endpoint_commands::EndpointCommands::default();

    // Spawn the stdin reader thread.
    let will_query_host_terminal_theme =
        state.attach_escape.is_none() && should_query_host_terminal_theme();
    // Terminals that report no pixel size through the ioctl are asked directly
    // instead of falling back to an assumed cell size.
    let will_query_host_cell_size = state.attach_escape.is_none()
        && host_cell_size_query_required(state.pixel_geometry_enabled);
    let stdin_quit = Arc::clone(&should_quit);
    let stdin_mouse_capture_active = Arc::clone(&host_mouse_capture_active);
    let stdin_sgr_pixels_active = Arc::clone(&host_sgr_pixels_active);
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
        if state.shell.is_some() {
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
    let pixel_geometry_enabled = state.pixel_geometry_enabled;
    let pixel_geometry_fallback = config.pixel_geometry_fallback;
    std::thread::spawn(move || {
        resize_poll_loop(
            &resize_tx,
            cols,
            rows,
            initial_cell_width_px,
            initial_cell_height_px,
            initial_pixel_geometry_exact,
            pixel_geometry_enabled,
            pixel_geometry_fallback,
            &resize_cell_size,
            &resize_quit,
        );
    });

    let mut write_stream = if let Some((stream, _handshake)) = initial {
        let max_frame_size = crate::protocol::MAX_FRAME_SIZE;
        let surface_decoder = protocol::surface_reuse::Decoder::default();
        let transport = start_endpoint_transport(
            stream,
            (),
            &event_tx,
            endpoint::ClientEndpointId::Local,
            1,
            max_frame_size,
            surface_decoder,
        )?;
        let mut registry = endpoint::EndpointRegistry::new(transport, 1);
        if state.shell.is_some() {
            registry.send(&ClientMessage::ClientShellFocus { focused: true });
        }
        registry
    } else {
        endpoint::EndpointRegistry::empty()
    };
    let mut supervisors = endpoint::EndpointSupervisors::with_ssh_settings(
        &config.paths,
        &endpoint_catalog.ssh,
        crate::remote::SavedSshSettings {
            manage_ssh_config: config.manage_ssh_config,
        },
        std::time::Instant::now(),
    );
    if federated {
        supervisors.add_local(
            client_socket_path(&config.paths),
            write_stream
                .connection(&endpoint::ClientEndpointId::Local)
                .map(|connection| connection.generation),
            std::time::Instant::now(),
        );
    }
    if local_unavailable
        && let Some(frame) = state
            .shell
            .as_mut()
            .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    {
        state.present_frame(frame);
    }
    let mut next_surface_serial = 1_u64;
    let mut pending_activation: Option<endpoint::PendingEndpointActivation> = None;
    let mut scheduled_activation = None;
    let mut selection = endpoint_selection::EndpointSelectionTracker::new(&endpoint_catalog);

    // Main event loop.
    let mut client_timer = timer::ClientLoopTimer::new();
    while !should_quit.load(Ordering::Acquire) {
        // Handoffs finish or roll back in many places; judge the requested selection once
        // nothing is in flight, so a rolled-back target neither stays selected nor persists.
        selection.settle_and_persist(
            &endpoint_catalog,
            pending_activation.is_some()
                || state.deferred_local_activation.is_some()
                || scheduled_activation.is_some(),
            write_stream.active_id(),
            write_stream
                .connection(write_stream.active_id())
                .is_some_and(|connection| connection.surface_active),
        );
        if scheduled_activation.is_none() {
            scheduled_activation = stale_freeze_recovery(
                &state,
                &write_stream,
                &selection.selected_endpoint(),
                pending_activation.is_some() || state.deferred_local_activation.is_some(),
                &mut freeze_recovery_attempted,
            );
        }
        if let Some(shell) = state.shell.as_ref() {
            supervisors.spawn_due(
                std::time::Instant::now(),
                endpoint::EndpointConnectOptions {
                    cols: state.reported_size.0,
                    rows: state.reported_size.1,
                    cell_width_px: state.reported_cell_size.0.min(protocol::MAX_CELL_SIZE_PX),
                    cell_height_px: state.reported_cell_size.1.min(protocol::MAX_CELL_SIZE_PX),
                    pixel_geometry_exact: state.pixel_geometry_exact
                        && state.reported_cell_size.0 <= protocol::MAX_CELL_SIZE_PX
                        && state.reported_cell_size.1 <= protocol::MAX_CELL_SIZE_PX,
                    surface_size: shell.surface_size(state.reported_size.0, state.reported_size.1),
                    mouse_capture: state.shell_mouse_capture_preference,
                },
                &supervisor_tx,
            );
        }
        let timer_delay = state
            .shell
            .as_ref()
            .map_or(Duration::from_millis(100), |shell| {
                shell.timer_delay(std::time::Instant::now())
            });
        let timer_deadline = client_timer.deadline(std::time::Instant::now(), timer_delay);
        let immediate_event = scheduled_activation.take();
        let event = if let Some(event) = immediate_event {
            event
        } else {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(timer_deadline.into()) => ClientLoopEvent::Timer,
                ev = supervisor_rx.recv() => ev.map(ClientLoopEvent::EndpointSupervisor).unwrap_or(ClientLoopEvent::Timer),
                ev = event_rx.recv() => ev.unwrap_or(ClientLoopEvent::Timer),
            }
        };
        let now = std::time::Instant::now();

        match event {
            ClientLoopEvent::StdinInput(data) => {
                if state.shell.is_some() {
                    if will_query_host_cell_size {
                        let events = crate::raw_input::parse_raw_input_bytes_sync(&data);
                        if let Some((width_px, height_px)) = reported_cell_size_from_events(&events)
                        {
                            store_reported_cell_size(&reported_cell_size, width_px, height_px);
                        }
                    }
                    let events = crate::raw_input::parse_raw_input_bytes_sync(&data);
                    if crate::raw_input::events_require_host_mode_refresh(&events) {
                        refresh_host_mouse_capture(
                            state.mouse_capture_active,
                            host_sgr_pixels_active.load(Ordering::Acquire),
                        );
                    }
                    let Some(shell) = state.shell.as_mut() else {
                        continue;
                    };
                    let outcome = shell.handle_raw_events(events);
                    let frame = outcome
                        .repaint
                        .then(|| shell.compose(state.reported_size.0, state.reported_size.1))
                        .flatten();
                    if finish_client_shell_input(
                        &mut state,
                        outcome,
                        frame,
                        &mut write_stream,
                        &mut pending_activation,
                        &mut endpoint_commands,
                        &mut scheduled_activation,
                    )? {
                        return Ok(());
                    }
                    continue;
                }
                let data = if let Some(attach_escape) = &mut state.attach_escape {
                    match attach_escape.filter_input(
                        data,
                        state.reported_size.1,
                        state.mouse_scroll_lines,
                    ) {
                        AttachInputAction::Forward(data) => data,
                        // Registry sends cannot fail here: a failed write is recorded against
                        // its endpoint and the timer's `take_failures` pass ends a
                        // non-federated client whose Local connection broke.
                        AttachInputAction::ForwardPair(first, second) => {
                            for data in [first, second] {
                                if let Some(notice) =
                                    attach::forward_input(&mut write_stream, &data).notice()
                                {
                                    remember_direct_notice(direct_notices, notice);
                                }
                            }
                            continue;
                        }
                        AttachInputAction::Semantic(action) => {
                            if let Some(message) = attach_semantic_message(action) {
                                write_stream.send(&message);
                            }
                            continue;
                        }
                        AttachInputAction::ForwardThenSemantic(prefix, action) => {
                            if let Some(notice) =
                                attach::forward_input(&mut write_stream, &prefix).notice()
                            {
                                remember_direct_notice(direct_notices, notice);
                            }
                            if let Some(message) = attach_semantic_message(action) {
                                write_stream.send(&message);
                            }
                            continue;
                        }
                        AttachInputAction::Detach => {
                            let _ = write_to_server(&mut write_stream, &ClientMessage::Detach);
                            return Ok(());
                        }
                        AttachInputAction::ForwardThenDetach(data) => {
                            if let Some(notice) =
                                attach::forward_input(&mut write_stream, &data).notice()
                            {
                                remember_direct_notice(direct_notices, notice);
                            }
                            let _ = write_to_server(&mut write_stream, &ClientMessage::Detach);
                            return Ok(());
                        }
                        AttachInputAction::None => continue,
                    }
                } else {
                    let events = crate::raw_input::parse_raw_input_bytes_sync(&data);
                    if crate::raw_input::events_require_host_surface_redraw(
                        &events,
                        state.redraw_on_focus_gained,
                    ) {
                        state.request_repaint();
                    }
                    if crate::raw_input::events_require_host_terminal_appearance_query(&events) {
                        query_host_terminal_appearance();
                    }
                    if crate::raw_input::events_require_host_terminal_theme_query(&events) {
                        query_host_terminal_theme();
                    }
                    if let Some((width_px, height_px)) = reported_cell_size_from_events(&events) {
                        store_reported_cell_size(&reported_cell_size, width_px, height_px);
                    }
                    data
                };
                if let Some(notice) = attach::forward_input(&mut write_stream, &data).notice() {
                    remember_direct_notice(direct_notices, notice);
                }
            }
            ClientLoopEvent::PixelMouse(data, geometry) => {
                if let Some(shell) = state.shell.as_mut() {
                    let outcome = shell.handle_pixel_mouse(&data, geometry);
                    let frame = outcome
                        .repaint
                        .then(|| shell.compose(state.reported_size.0, state.reported_size.1))
                        .flatten();
                    if finish_client_shell_input(
                        &mut state,
                        outcome,
                        frame,
                        &mut write_stream,
                        &mut pending_activation,
                        &mut endpoint_commands,
                        &mut scheduled_activation,
                    )? {
                        return Ok(());
                    }
                    continue;
                }
                if let Some(attach_escape) = state.attach_escape.as_mut() {
                    if let Some(prefix) = attach_escape.take_pending_prefix()
                        && let Some(notice) =
                            attach::forward_input(&mut write_stream, &prefix).notice()
                    {
                        remember_direct_notice(direct_notices, notice);
                    }
                    if let Some((kind, position, modifiers)) =
                        direct_attach_pixel_mouse(&data, geometry)
                    {
                        let message = ClientMessage::AttachMouse {
                            kind,
                            position,
                            geometry: Some(crate::protocol::ClientMouseGeometry {
                                cols: geometry.cols,
                                rows: geometry.rows,
                                width_px: geometry.width_px,
                                height_px: geometry.height_px,
                            }),
                            modifiers,
                            lines: u16::try_from(state.mouse_scroll_lines.max(1))
                                .unwrap_or(u16::MAX),
                        };
                        write_stream.send(&message);
                    }
                }
            }
            ClientLoopEvent::TerminalUnavailable(err) => {
                info!(err = %err, "client terminal unavailable; detaching");
                let _ = write_to_server(&mut write_stream, &ClientMessage::Detach);
                return Ok(());
            }
            ClientLoopEvent::Resize(
                new_cols,
                new_rows,
                cell_width_px,
                cell_height_px,
                pixel_geometry_exact,
            ) => {
                let (cell_width_px, cell_height_px, pixel_geometry_exact) =
                    terminal_geometry::bounded_cell_geometry(
                        cell_width_px,
                        cell_height_px,
                        pixel_geometry_exact,
                    );
                if let Some((enabled, sgr_pixels)) = host_mouse_capture_update(
                    host_mouse_capture_active.load(Ordering::Acquire),
                    host_sgr_pixels_active.load(Ordering::Acquire),
                    state.mouse_capture_active,
                    state.endpoint_sgr_pixels_requested,
                    pixel_geometry_exact,
                ) {
                    set_mouse_capture(enabled, sgr_pixels).map_err(ClientError::HostTerminal)?;
                    host_mouse_capture_active.store(enabled, Ordering::Release);
                    host_sgr_pixels_active.store(sgr_pixels, Ordering::Release);
                }
                state.reported_size = if client_shell_size {
                    let bounded = protocol::ClientSurfaceSize {
                        cols: new_cols,
                        rows: new_rows,
                    }
                    .clamped();
                    (bounded.cols, bounded.rows)
                } else {
                    (new_cols, new_rows)
                };
                state.reported_cell_size = (cell_width_px, cell_height_px);
                state.pixel_geometry_exact = pixel_geometry_exact;
                // Resizing invalidates the host-side blit baseline. The retained pane surface
                // stays: until the resized one arrives, `compose` draws it clipped to the new
                // pane area (with pane hits clipped to match) instead of dropping to the
                // machine-list placeholder.
                state.request_repaint();
                let msg = if let Some(shell) = &state.shell {
                    client_shell_resize_message(
                        shell,
                        state.reported_size.0,
                        state.reported_size.1,
                        cell_width_px,
                        cell_height_px,
                        pixel_geometry_exact,
                    )
                } else {
                    ClientMessage::Resize {
                        cols: new_cols,
                        rows: new_rows,
                        cell_width_px,
                        cell_height_px,
                        pixel_mouse: pixel_geometry_exact,
                    }
                };
                if let Some(activation) = pending_activation.as_mut() {
                    if let Err(error) = activation.update_resize(&msg, &mut write_stream) {
                        rollback_endpoint_activation(
                            &mut state,
                            &mut write_stream,
                            &mut pending_activation,
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
                if let Some(frame) = state
                    .shell
                    .as_mut()
                    .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
                {
                    state.present_chrome(frame, pending_activation.is_some());
                }
            }
            ClientLoopEvent::EndpointSupervisor(event) => match event {
                endpoint::EndpointSupervisorEvent::Status {
                    endpoint_id,
                    generation,
                    status,
                    message,
                } => {
                    if !supervisors.record_status(&endpoint_id, generation, status, now) {
                        continue;
                    }
                    if status == endpoint::ClientEndpointStatus::Attention {
                        warn!(endpoint = %endpoint_id.storage_key(), generation, error = %message, "endpoint needs attention");
                    }
                    let unavailable = state.shell.as_mut().and_then(|shell| {
                        shell.set_endpoint_status(&endpoint_id, status);
                        shell.set_machine_diagnostic(&endpoint_id, &message);
                        (status == endpoint::ClientEndpointStatus::Attention
                            && shell.endpoint_is_active(&endpoint_id))
                        .then(|| format!("{}: {message}", shell.endpoint_label(&endpoint_id)))
                    });
                    if let Some(message) = unavailable {
                        present_handoff_unavailable(&mut state, message);
                    } else if let Some(frame) = state.shell.as_mut().and_then(|shell| {
                        shell.compose(state.reported_size.0, state.reported_size.1)
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
                        continue;
                    }
                    let frame = state.shell.as_mut().and_then(|shell| {
                        shell.compose(state.reported_size.0, state.reported_size.1)
                    });
                    let reader_quit = writer.stop_handle();
                    write_stream.insert(endpoint_id.clone(), writer, generation, false);
                    if let Some(frame) = frame {
                        // Connecting changes no pane projection (the connection has no
                        // surface yet), only the machine list.
                        state.present_chrome(frame, pending_activation.is_some());
                    }
                    let surface_decoder = protocol::surface_reuse::Decoder::default();
                    let reader_tx = event_tx.clone();
                    std::thread::spawn(move || {
                        server_reader_thread(
                            reader,
                            &reader_tx,
                            &reader_quit,
                            MAX_FRAME_SIZE,
                            endpoint_id,
                            generation,
                            surface_decoder,
                        );
                    });
                }
            },
            ClientLoopEvent::ActivateEndpoint {
                endpoint_id,
                target,
                force,
            } => {
                let generation = write_stream
                    .connection(&endpoint_id)
                    .map(|connection| connection.generation);
                // Persisting waits for the handoff to commit; see `endpoint_selection`.
                if !selection.begin(&endpoint_catalog, &endpoint_id, generation) {
                    continue;
                }
                begin_endpoint_activation(
                    &mut state,
                    &mut write_stream,
                    &mut endpoint_commands,
                    &mut pending_activation,
                    &mut next_surface_serial,
                    endpoint_id,
                    target,
                    force,
                    now,
                    &mut scheduled_activation,
                )?;
            }
            ClientLoopEvent::ServerMessage {
                endpoint_id,
                generation,
                message,
            } => {
                if !write_stream.accepts(&endpoint_id, generation) {
                    continue;
                }
                write_stream.received(&endpoint_id, generation, now);
                let endpoint_active = write_stream.active_id() == &endpoint_id
                    && write_stream
                        .connection(&endpoint_id)
                        .is_some_and(|connection| connection.surface_active);
                let activation_message = pending_activation
                    .as_ref()
                    .is_some_and(|pending| pending.accepts_endpoint(&endpoint_id, generation));
                let command_response = match message.as_ref() {
                    ServerMessage::ClientShellEndpointResponseChunk {
                        boot_id,
                        request_id,
                        ..
                    } => endpoint_commands.accepts_response(
                        &endpoint_id,
                        generation,
                        boot_id,
                        request_id,
                    ),
                    _ => false,
                };
                if !endpoint::accepts_endpoint_message(
                    endpoint_active,
                    activation_message,
                    command_response,
                    message.as_ref(),
                ) {
                    continue;
                }
                // Target presentation effects may arrive as soon as surface.set(true) is
                // acknowledged. They cannot be applied while the source frame is frozen; the
                // target receives one explicit replay after the coherent commit instead.
                if state.presentation_frozen
                    && activation_message
                    && endpoint::is_presentation_effect(message.as_ref())
                {
                    continue;
                }
                match *message {
                    ServerMessage::PaneSurface(surface) => {
                        if activation_message {
                            let progress = pending_activation.as_mut().map(|pending| {
                                pending.receive_surface(&endpoint_id, generation, surface)
                            });
                            if matches!(progress, Some(endpoint::SurfaceActivationProgress::Ready))
                                && let Some(event) = complete_endpoint_activation(
                                    &mut state,
                                    &mut write_stream,
                                    &mut pending_activation,
                                    &mut endpoint_commands,
                                )?
                            {
                                scheduled_activation = Some(event);
                            }
                            continue;
                        }
                        // A frozen presentation keeps its pane projection: chrome frames pass
                        // the freeze (`present_frozen_chrome`) and would otherwise carry this
                        // surface with them. The handoff commit that unfreezes installs its own.
                        if !endpoint_active || state.presentation_frozen {
                            continue;
                        }
                        let composed = if let Some(shell) = &mut state.shell {
                            shell.set_pane_surface(surface);
                            shell.compose(state.reported_size.0, state.reported_size.1)
                        } else {
                            None
                        };
                        if let Some(frame) = composed {
                            state.present_frame(frame);
                        }
                    }
                    ServerMessage::PaneSurfacePatch(patch) => {
                        // Same rule as full surfaces above. Dropping a patch leaves the
                        // retained surface behind the server's, which is harmless here: the
                        // handoff commit that ends the freeze replaces the surface outright.
                        if state.presentation_frozen {
                            continue;
                        }
                        let outcome = state
                            .shell
                            .as_mut()
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
                            let composed = state.shell.as_mut().and_then(|shell| {
                                shell.compose(state.reported_size.0, state.reported_size.1)
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
                        if !federated && endpoint_id.is_local() {
                            return Err(ClientError::ServerShutdown { reason });
                        }
                        write_stream.fail(
                            &endpoint_id,
                            &io::Error::new(
                                io::ErrorKind::ConnectionAborted,
                                reason.unwrap_or_else(|| "server stopped".into()),
                            ),
                        );
                    }
                    ServerMessage::ClientShellError { message } => {
                        if let Some(shell) = state.shell.as_mut()
                            && shell.receive_endpoint_error(message)
                            && let Some(frame) =
                                shell.compose(state.reported_size.0, state.reported_size.1)
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
                            pending.accepts_response(
                                &endpoint_id,
                                generation,
                                &boot_id,
                                &request_id,
                            )
                        }) {
                            if !final_chunk {
                                rollback_endpoint_activation(
                                    &mut state,
                                    &mut write_stream,
                                    &mut pending_activation,
                                    "endpoint returned a chunked activation acknowledgement",
                                    false,
                                );
                                continue;
                            }
                            let progress = pending_activation.as_mut().map(|pending| {
                                pending.receive_response_for_boot(
                                    &endpoint_id,
                                    generation,
                                    &boot_id,
                                    &request_id,
                                    &data,
                                    &mut write_stream,
                                )
                            });
                            match progress {
                                Some(endpoint::SurfaceActivationProgress::Ready) => {
                                    if let Some(event) = complete_endpoint_activation(
                                        &mut state,
                                        &mut write_stream,
                                        &mut pending_activation,
                                        &mut endpoint_commands,
                                    )? {
                                        scheduled_activation = Some(event);
                                    }
                                }
                                Some(endpoint::SurfaceActivationProgress::Rejected {
                                    message,
                                    source_release_rejected,
                                }) => {
                                    rollback_endpoint_activation(
                                        &mut state,
                                        &mut write_stream,
                                        &mut pending_activation,
                                        &message,
                                        source_release_rejected,
                                    );
                                }
                                _ => {}
                            }
                            continue;
                        }
                        if request_id.starts_with("client-shell-surface:") {
                            continue;
                        }
                        let completed = endpoint_commands.receive_chunk(
                            &endpoint_id,
                            generation,
                            &boot_id,
                            &request_id,
                            final_chunk,
                            data,
                        );
                        let Some(completed) = completed else {
                            continue;
                        };
                        // The whole outcome goes through `finish_client_shell_input`: a
                        // copy-mode response replays keys queued while it was in flight,
                        // and those can carry pane input, a resize or a detach. It also
                        // releases the next queued command in this endpoint's lane.
                        let (outcome, frame) = match state.shell.as_mut() {
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
                                        repaint: shell
                                            .cancel_endpoint_request(&completed.request_id),
                                        ..Default::default()
                                    }
                                };
                                let frame = outcome
                                    .repaint
                                    .then(|| {
                                        shell.compose(state.reported_size.0, state.reported_size.1)
                                    })
                                    .flatten();
                                (outcome, frame)
                            }
                            None => (shell::ClientShellInput::default(), None),
                        };
                        if finish_client_shell_input(
                            &mut state,
                            outcome,
                            frame,
                            &mut write_stream,
                            &mut pending_activation,
                            &mut endpoint_commands,
                            &mut scheduled_activation,
                        )? {
                            return Ok(());
                        }
                    }
                    ServerMessage::DirectTerminalNotice { message } => {
                        if state.attach_escape.is_some() {
                            remember_direct_notice(direct_notices, message);
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
                        state.window_title_written = true;
                        let _ = crate::terminal_effects::write_window_title(
                            &mut io::stdout(),
                            title.as_deref(),
                        );
                    }
                    ServerMessage::MouseCapture {
                        enabled,
                        sgr_pixels,
                    } => {
                        state.endpoint_mouse_capture_requested = enabled;
                        state.endpoint_sgr_pixels_requested = sgr_pixels;
                        let enabled =
                            effective_mouse_capture(enabled, state.direct_mouse_capture_preference);
                        let update = host_mouse_capture_update(
                            host_mouse_capture_active.load(Ordering::Acquire),
                            host_sgr_pixels_active.load(Ordering::Acquire),
                            enabled,
                            sgr_pixels,
                            state.pixel_geometry_exact,
                        );
                        let next_sgr_pixels = effective_sgr_pixel_mouse(
                            enabled,
                            sgr_pixels,
                            state.pixel_geometry_exact,
                        );
                        if let Some((enabled, sgr_pixels)) = update {
                            set_mouse_capture(enabled, sgr_pixels)
                                .map_err(ClientError::HostTerminal)?;
                        }
                        state.mouse_capture_active = enabled;
                        host_mouse_capture_active.store(enabled, Ordering::Release);
                        host_sgr_pixels_active.store(next_sgr_pixels, Ordering::Release);
                    }
                    ServerMessage::DirectTerminalKeyboardProtocol {
                        flags,
                        modify_other_keys_level,
                    } => {
                        if state.attach_escape.is_some() {
                            crate::terminal_modes::set_direct_host_keyboard_protocol(
                                &mut io::stdout(),
                                &mut state.direct_keyboard_protocol,
                                flags,
                                modify_other_keys_level,
                            )
                            .map_err(ClientError::HostTerminal)?;
                        }
                    }
                    ServerMessage::ClientShellKeyboardReportAll { enabled } => {
                        if state.shell.is_some() {
                            state.pane_keyboard_report_all = enabled;
                            sync_client_shell_keyboard_report_all(&mut state)?;
                        }
                    }
                    ServerMessage::EndpointControl { kind, data } => {
                        if kind == crate::protocol::endpoint::PRESENTATION_EFFECTS_READY_KIND {
                            let progress = pending_activation.as_mut().map(|activation| {
                                activation.receive_presentation_effects_ready(
                                    &endpoint_id,
                                    generation,
                                    &data,
                                )
                            });
                            if matches!(progress, Some(endpoint::SurfaceActivationProgress::Ready))
                                && let Some(event) = complete_endpoint_activation(
                                    &mut state,
                                    &mut write_stream,
                                    &mut pending_activation,
                                    &mut endpoint_commands,
                                )?
                            {
                                scheduled_activation = Some(event);
                            }
                            continue;
                        }
                        let snapshot = match endpoint::decode_endpoint_control(&kind, &data) {
                            Ok(endpoint::EndpointControlMessage::HealthPong) => continue,
                            Ok(endpoint::EndpointControlMessage::Ignored) => {
                                debug!(%kind, "ignoring unknown endpoint control message");
                                continue;
                            }
                            Ok(endpoint::EndpointControlMessage::Snapshot(snapshot)) => snapshot,
                            Err(message)
                                if federated
                                    || !endpoint::protocol_failure_is_fatal(&endpoint_id) =>
                            {
                                if handle_endpoint_attention(
                                    &mut state,
                                    &mut write_stream,
                                    &mut endpoint_commands,
                                    &mut supervisors,
                                    &mut pending_activation,
                                    &endpoint_id,
                                    generation,
                                    now,
                                    &message,
                                ) {
                                    clear_endpoint_host_effects(
                                        &mut state,
                                        &host_mouse_capture_active,
                                        &host_sgr_pixels_active,
                                    );
                                }
                                continue;
                            }
                            Err(message) => {
                                return Err(ClientError::Protocol(protocol::FramingError::Io(
                                    io::Error::new(io::ErrorKind::InvalidData, message),
                                )));
                            }
                        };
                        let projection_pending = activation_message;
                        let activation_progress = activation_message
                            .then(|| {
                                pending_activation.as_mut().map(|pending| {
                                    pending.receive_snapshot(&endpoint_id, generation, &snapshot)
                                })
                            })
                            .flatten();
                        install_client_shell_snapshot(
                            &mut state,
                            &endpoint_id,
                            snapshot,
                            projection_pending,
                            &mut write_stream,
                        )?;
                        if matches!(
                            activation_progress,
                            Some(endpoint::SurfaceActivationProgress::Ready)
                        ) && let Some(event) = complete_endpoint_activation(
                            &mut state,
                            &mut write_stream,
                            &mut pending_activation,
                            &mut endpoint_commands,
                        )? {
                            scheduled_activation = Some(event);
                        }
                        write_stream.mark_ready(&endpoint_id, generation);
                        if endpoint_id.is_local()
                            && let Some(event) =
                                take_ready_local_activation(&mut state, &write_stream)
                        {
                            scheduled_activation = Some(event);
                            continue;
                        }
                        let selected_endpoint = selection.selected_endpoint();
                        let activation_ready = state.shell.as_ref().is_some_and(|shell| {
                            shell.endpoint_has_snapshot(&selected_endpoint)
                                && (!write_stream
                                    .connection(write_stream.active_id())
                                    .is_some_and(|connection| connection.surface_active)
                                    || shell.endpoint_boot_id(write_stream.active_id()).is_some())
                        });
                        let selected_connection = write_stream.connection(&selected_endpoint);
                        let needs_surface = selected_connection
                            .is_some_and(|connection| !connection.surface_active);
                        // A handoff to this connection already failed; retrying it on every
                        // snapshot would freeze input and roll back again each time.
                        let retry_suppressed = selection.suppresses(
                            &selected_endpoint,
                            selected_connection.map(|connection| connection.generation),
                        );
                        if activation_ready
                            && needs_surface
                            && !retry_suppressed
                            && pending_activation.is_none()
                            && state.deferred_local_activation.is_none()
                        {
                            scheduled_activation = Some(ClientLoopEvent::ActivateEndpoint {
                                endpoint_id: selected_endpoint,
                                target: None,
                                force: false,
                            });
                        }
                    }
                    ServerMessage::Welcome { .. } => {
                        debug!("received unexpected Welcome in main loop");
                    }
                }
            }
            ClientLoopEvent::ServerDisconnected {
                endpoint_id,
                generation,
                error,
            } => {
                if !write_stream.accepts(&endpoint_id, generation) {
                    continue;
                }
                write_stream.fail(&endpoint_id, &error);
            }
            ClientLoopEvent::Timer => {
                client_timer.fired();
                write_stream.tick_health(now);
                for failure in write_stream.take_failures() {
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
                    if !federated && failure.endpoint_id.is_local() {
                        return Err(ClientError::ConnectionLost(io::Error::new(
                            failure.kind,
                            failure.message,
                        )));
                    }
                    if handle_endpoint_disconnect(
                        &mut state,
                        &mut write_stream,
                        &mut endpoint_commands,
                        &mut supervisors,
                        &mut pending_activation,
                        &failure.endpoint_id,
                        failure.generation,
                        now,
                        &format!("{}; reconnecting", failure.message),
                    ) {
                        clear_endpoint_host_effects(
                            &mut state,
                            &host_mouse_capture_active,
                            &host_sgr_pixels_active,
                        );
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
                        .shell
                        .as_ref()
                        .map(|shell| shell.endpoint_label(&endpoint_id).to_owned())
                        .unwrap_or_else(|| "Endpoint".into());
                    rollback_endpoint_activation(
                        &mut state,
                        &mut write_stream,
                        &mut pending_activation,
                        &format!("{label} did not produce a coherent surface in time"),
                        false,
                    );
                }
                match catalog_watch.as_mut().and_then(|watch| watch.poll(now)) {
                    Some(Ok(profiles)) => {
                        let active_retired = follow_endpoint_catalog(
                            &mut state,
                            &mut write_stream,
                            &mut endpoint_commands,
                            &mut supervisors,
                            &mut pending_activation,
                            &mut endpoint_catalog,
                            &config.local_socket_path,
                            profiles,
                            now,
                        );
                        selection.catalog_changed(&endpoint_catalog);
                        if active_retired {
                            clear_endpoint_host_effects(
                                &mut state,
                                &host_mouse_capture_active,
                                &host_sgr_pixels_active,
                            );
                            if scheduled_activation.is_none() {
                                scheduled_activation = Some(ClientLoopEvent::ActivateEndpoint {
                                    endpoint_id: endpoint::ClientEndpointId::Local,
                                    target: None,
                                    force: false,
                                });
                            }
                        }
                        federated = endpoint_catalog.has_ssh();
                    }
                    Some(Err(error)) => {
                        warn!(%error, "saved SSH endpoint catalog changed but is unusable; keeping the machines already loaded");
                    }
                    None => {}
                }
                if state.shell.is_some() {
                    let expired_endpoints = endpoint_commands
                        .expire(now)
                        .into_iter()
                        .filter(|expired| {
                            write_stream.accepts(&expired.endpoint_id, expired.generation)
                        })
                        .collect::<Vec<_>>();
                    let Some(shell) = state.shell.as_mut() else {
                        continue;
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
                            .then(|| shell.compose(state.reported_size.0, state.reported_size.1))
                            .flatten();
                        (outcome, frame)
                    };
                    if finish_client_shell_input(
                        &mut state,
                        outcome,
                        frame,
                        &mut write_stream,
                        &mut pending_activation,
                        &mut endpoint_commands,
                        &mut scheduled_activation,
                    )? {
                        return Ok(());
                    }
                }
            }
        }
    }

    // Clean exit (Ctrl+C). Send Detach before closing.
    let detach = ClientMessage::Detach;
    let _ = write_to_server(&mut write_stream, &detach);
    let _ = io::stdout().flush();

    Ok(())
}

#[cfg(test)]
mod tests;
