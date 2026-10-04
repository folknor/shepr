use crate::client_loop::{ClientLoop, EventQueue, HostCellReport, LoopSignals};
use crate::endpoint::connection_io::{
    EndpointConnectionIo, LocalAttachFailure, attach_local_endpoint,
};
use crate::errors::{ClientExit, ClientRunError, LoopExit, endpoint_setup_launch_error};
use crate::events::ClientLoopEvent;
use crate::limits::{
    CLIENT_EVENT_QUEUE_CAPACITY, CLIENT_RUNTIME_SHUTDOWN_TIMEOUT, SSH_RESOURCE_RELEASE_TIMEOUT,
};
use crate::loop_config::ClientSettings;
use crate::shell_runtime::view_geometry;
use crate::state::{ClientState, HostWriteFailure};
use crate::terminal_geometry::{
    AtomicCellSize, HostGeometrySnapshot, SharedHostGeometry, initial_terminal_geometry,
    query_host_cell_size, query_host_terminal_appearance, query_host_terminal_theme,
    resize_poll_loop,
};
use crate::terminal_setup::{TerminalGuard, setup_terminal, should_draw_host_cursor};
use crate::{endpoint, fatal_panic, input, shell, state, terminal_geometry, terminal_setup};
use shepr_protocol::ClientMessage;
use shepr_termio::blit as render_ansi;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// What launch prepares before taking the terminal: settings, supervisors, the event channel
/// and the first Local attachment. Every launch-fatal configuration and endpoint check runs
/// while building it; runtime creation and terminal setup follow.
struct Launched {
    launch_now: std::time::Instant,
    initial: LocalAtLaunch,
    supervisors: endpoint::EndpointSupervisors,
    machines: Vec<shepr_config::MachineConfig>,
    local_failure_policy: endpoint::LocalFailurePolicy,
    initial_host_geometry: HostGeometrySnapshot,
    /// `initial_host_geometry`'s geometry, bounded once
    /// (`terminal_geometry::bounded_cell_geometry`) for the first attach and the client state.
    initial_geometry: terminal_geometry::TerminalGeometry,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    event_rx: tokio::sync::mpsc::Receiver<ClientLoopEvent>,
    settings: ClientSettings,
    shell_config: shell::ClientShellConfig,
    paths: shepr_paths::AppPaths,
    local_mismatch_guidance: Arc<str>,
}

impl Launched {
    fn prepare(
        config: &shepr_config::ValidatedClientConfig,
        paths: &shepr_paths::AppPaths,
        connectors: Vec<shepr_remote::MachineSshConnector>,
    ) -> Result<Self, ClientRunError> {
        let settings = ClientSettings::resolve(config).map_err(io::Error::from)?;
        let socket_path = paths.server_address().socket().to_path_buf();
        let shell_config = shell::ClientShellConfig::from_validated_config(config)
            .with_local_endpoint(paths.state_dir(), &socket_path)
            .map_err(io::Error::from)?;
        let mismatch_guidance: Arc<str> =
            shepr_launch::guidance::build_mismatch_guidance(paths.server_address()).into();

        crate::logging::startup();
        info!(path = %socket_path.display(), "connecting to server");

        let machines = config.machines().to_vec();
        let local_failure_policy = endpoint::LocalFailurePolicy::for_machines(&machines);
        // A machine whose connector cannot be built fails the launch here, before the Local
        // handshake and before the terminal is taken.
        // client-clock-sample-ok: supervisor creation time, before terminal ownership.
        let launch_now = std::time::Instant::now();
        let supervisors = endpoint::EndpointSupervisors::new(connectors, launch_now)
            .map_err(|error| endpoint_setup_launch_error(&error))?;
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel::<ClientLoopEvent>(CLIENT_EVENT_QUEUE_CAPACITY);

        // Get the terminal geometry before handshake (before raw mode).
        let initial_host_geometry = initial_terminal_geometry()?;
        let initial_geometry =
            terminal_geometry::bounded_cell_geometry(initial_host_geometry.geometry);
        let shell_surface_size =
            shell_config.initial_surface_size(initial_geometry.cols(), initial_geometry.rows());
        // Healthy Local attaches directly; only an actual failure enters background recovery.
        let local_generation = endpoint::EndpointSupervisors::initial_local_generation();
        let initial_attach = attach_local_endpoint(
            &socket_path,
            shepr_protocol::endpoint::EndpointClientHello {
                geometry: view_geometry(initial_geometry, shell_surface_size),
                mouse_capture: settings.mouse_capture_active(),
                // Local is the first shown endpoint; it can render as soon as the handshake completes.
                surface_active: true,
            },
            &mismatch_guidance,
        )
        .and_then(|accepted| {
            EndpointConnectionIo::start(
                accepted,
                &event_tx,
                endpoint::ClientEndpointId::Local,
                local_generation,
            )
            .map_err(LocalAttachFailure::Setup)
        });
        let reconnect_local = local_failure_policy.reconnects_local();
        let initial = match initial_attach {
            Ok(endpoint) => LocalAtLaunch::Attached {
                endpoint,
                generation: local_generation,
            },
            Err(failure) if reconnect_local => {
                match &failure {
                    LocalAttachFailure::Connection(error) => {
                        warn!(%error, "Local is unavailable; keeping configured machines available");
                    }
                    LocalAttachFailure::Handshake(error) => {
                        warn!(%error, "Local handshake failed; keeping configured machines available");
                    }
                    LocalAttachFailure::Setup(error) => {
                        warn!(%error, "Local transport setup failed; keeping configured machines available");
                    }
                }
                // The Local endpoint's first status comes from this real
                // attach, not from the launch's `LaunchError`: the binary
                // prints a launch failure taken with machines configured as a
                // notice before the terminal is taken, and the attach here
                // yields its own typed `LocalAttachFailure`, so the status is
                // not seeded from `LaunchError`.
                LocalAtLaunch::Failed {
                    failure: failure.initial_failure(),
                    generation: local_generation,
                }
            }
            Err(failure) => return Err(failure.into_launch_error()),
        };

        Ok(Self {
            launch_now,
            initial,
            supervisors,
            machines,
            local_failure_policy,
            initial_host_geometry,
            initial_geometry,
            event_tx,
            event_rx,
            settings,
            shell_config,
            paths: paths.clone(),
            local_mismatch_guidance: mismatch_guidance,
        })
    }
}

/// Runs the local shell client with startup settings already loaded by the
/// launch coordinator. The binary launcher installs the process-wide file
/// logger before calling this function. The machines are the launch
/// config's `[[machines]]`, fixed for the life of the client, and `connectors`
/// holds one connector per machine.
pub(crate) fn run_launched_client(
    config: &shepr_config::ValidatedClientConfig,
    paths: &shepr_paths::AppPaths,
    connectors: Vec<shepr_remote::MachineSshConnector>,
) -> Result<ClientExit, ClientRunError> {
    // A panic on any thread from here on ends the client through the one finalization
    // below (see `fatal_panic`). The guard and the runtime live outside the caught launch,
    // so an unwinding launch drops neither: the finalization restores the one and shuts the
    // other down with its bound. A panic inside terminal setup itself is restored by the
    // half-built guard's drop. Every outcome, a failure before the terminal is taken
    // included, goes through that finalization.
    let fatal = fatal_panic::FatalPanic::install();
    let should_quit = Arc::new(AtomicBool::new(false));
    let mut terminal_slot: Option<TerminalGuard> = None;
    let mut runtime_slot: Option<tokio::runtime::Runtime> = None;
    let launched = fatal.guard(|| -> Result<Result<(), LoopExit>, ClientRunError> {
        let launched = Launched::prepare(config, paths, connectors)?;
        // A runtime that cannot be built fails the launch before the terminal is taken.
        let runtime = runtime_slot.insert(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(io::Error::other)?,
        );
        // A shell with configured machines can show connection notices without a server snapshot.
        let (terminal_guard, output_writer) =
            setup_terminal(launched.settings.mouse_capture_active(), launched.settings.modify_other_keys_mode()).map_err(
                |err| io::Error::new(err.kind(), format!("failed to set up terminal: {err}")),
            )?;
        let terminal_guard = terminal_slot.insert(terminal_guard);
        // ctrlc's "termination" feature also catches SIGTERM/SIGHUP so direct
        // termination signals wake the event loop so it restores the terminal.
        let quit_flag = Arc::clone(&should_quit);
        let quit_event_tx = launched.event_tx.clone();
        if let Err(err) = ctrlc::set_handler(move || {
            quit_flag.store(true, Ordering::Release);
            quit_event_tx.try_send(ClientLoopEvent::Quit).ok();
        }) {
            warn!(error = %err, "failed to install termination handler; terminal restore relies on TerminalGuard::Drop");
        }

        let mut client_loop = match launched.into_loop(output_writer, terminal_guard, Arc::clone(&should_quit), Arc::clone(&fatal), HostHelpers::spawn_threads) {
            Ok(client_loop) => client_loop,
            Err(exit) => return Ok(Err(exit)),
        };
        Ok(runtime.block_on(client_loop.run()))
    });

    // The one finalization, whether the launch returned, failed or panicked.
    // Each stage runs even if an earlier one panics. The host helpers stop
    // first; the terminal is restored before the binary prints anything. A
    // restoration that panics is not retried: the restorer is what failed.
    should_quit.store(true, Ordering::Release);
    if let Some(guard) = terminal_slot.take() {
        // Each failed restoration step is logged by the restorer. It does not change
        // whether a lost server session was a failure.
        let _ = fatal.guard(|| guard.restore());
    }
    if let Some(runtime) = runtime_slot.take() {
        fatal.guard(|| runtime.shutdown_timeout(CLIENT_RUNTIME_SHUTDOWN_TIMEOUT));
    }
    fatal.guard(|| {
        shepr_remote::release_ssh_resources_before_exit(SSH_RESOURCE_RELEASE_TIMEOUT);
    });
    if let Some(diagnostic) = fatal.diagnostic() {
        fatal.guard(|| tracing::error!(diagnostic, "client panicked; exiting"));
    }
    fatal.guard(crate::logging::shutdown);

    // Read once, after finalization: a panic later than this cannot change
    // the outcome. The diagnostic reaches the restored screen through the
    // binary's exit lines.
    launch_outcome(launched, &fatal)
}

impl Launched {
    /// Builds the loop once the terminal is taken. Its failures (a host query write, a
    /// latched panic) are loop exits, reported after the terminal is restored.
    ///
    /// Uses a threaded architecture:
    /// - stdin reader thread -> sends parsed input events
    /// - resize poller thread -> sends resize events to main loop
    /// - endpoint reader threads -> read ServerMessages and send them to main loop
    /// - one event channel: host input, resize, endpoint readers, connection supervisors, and quit
    /// - main loop: coordinates input, output, and server communication
    ///
    /// `start_host_helpers` starts the stdin reader and the resize poller; launch passes
    /// [`HostHelpers::spawn_threads`], and tests a stand-in that reads no real terminal.
    fn into_loop(
        self,
        output_writer: terminal_setup::HostTerminalWriter,
        terminal_guard: &mut TerminalGuard,
        should_quit: Arc<AtomicBool>,
        fatal: Arc<fatal_panic::FatalPanic>,
        start_host_helpers: impl FnOnce(HostHelpers),
    ) -> Result<ClientLoop, LoopExit> {
        let Self {
            launch_now,
            initial,
            mut supervisors,
            machines,
            local_failure_policy,
            initial_host_geometry,
            initial_geometry,
            event_tx,
            event_rx,
            settings,
            shell_config,
            paths,
            local_mismatch_guidance,
        } = self;
        let (initial, local_generation, local_launch_state) = match initial {
            LocalAtLaunch::Attached {
                endpoint,
                generation,
            } => (Some(endpoint), generation, LocalLaunchState::Attached),
            LocalAtLaunch::Failed {
                failure,
                generation,
            } => (
                None,
                generation,
                failure.map_or(LocalLaunchState::Unreached, LocalLaunchState::Failed),
            ),
        };
        let host_geometry = SharedHostGeometry::new(initial_host_geometry);
        let draw_host_cursor = should_draw_host_cursor(settings.host_cursor());

        let host_modes = terminal_guard.host_modes();
        let mut state = ClientState {
            blit_encoder: render_ansi::BlitEncoder::new(),
            output_writer: Box::new(output_writer),
            host_modes,
            host_theme_updates: Vec::new(),
            reported_geometry: initial_geometry,
            settings,
            shell: Box::new(shell::ClientShellState::new_at(shell_config, launch_now)),
            repaint_pending: false,
            presentation_dirty: state::PresentationDirty::Clean,
            pending_surface_patch: None,
            draw_host_cursor,
            frame_write_failure: HostWriteFailure::default(),
            title_write_failure: HostWriteFailure::default(),
            mode_write_failure: HostWriteFailure::default(),
            retry_host_modes: false,
            refused_output_retry: state::RefusedOutputRetry::default(),
        };
        state.shell.endpoints.choice = match &local_launch_state {
            LocalLaunchState::Attached => {
                endpoint::EndpointChoice::showing(endpoint::ClientEndpointId::Local)
            }
            LocalLaunchState::Unreached | LocalLaunchState::Failed(_) => {
                endpoint::EndpointChoice::waiting_for(endpoint::ClientEndpointId::Local)
            }
        };
        state.shell.set_host_cell(initial_geometry.cell());
        state.shell.set_machines(&machines);
        if let LocalLaunchState::Failed(failure) = &local_launch_state {
            let status = endpoint::EndpointFailureStatus::after_failure(failure);
            state
                .shell
                .set_endpoint_status(&endpoint::ClientEndpointId::Local, status);
        }
        // Cell size reported by the host terminal, packed as width<<32 | height.
        // Zero means the host has not reported one.
        let reported_cell_size = Arc::new(AtomicCellSize::new());

        // Arm reply tracking only after the corresponding query was written successfully.
        let color_scheme_query =
            query_host_terminal_theme(&mut state.output_writer).map_err(LoopExit::HostTerminal)?;
        query_host_terminal_appearance(&mut state.output_writer).map_err(LoopExit::HostTerminal)?;
        // Terminals that report no pixel size through the ioctl are asked directly
        // instead of falling back to an assumed cell size.
        let cell_size_query = if initial_geometry.cell().is_exact() {
            input::ProbeAvailability::NotArmed
        } else {
            query_host_cell_size(&mut state.output_writer).map_err(LoopExit::HostTerminal)?
        };
        let probe = input::HostInputProbe {
            color_scheme_query,
            cell_size_query,
            mouse: state.host_modes.mouse_input_probe(),
            escape_disambiguation: terminal_guard.escape_disambiguation(),
        };

        // Start the host helpers after query writes so a failed write does not make
        // the stdin parser wait for a host reply that cannot arrive.
        // They share the event channel with the endpoint readers, supervisors and the
        // signal handler.
        start_host_helpers(HostHelpers {
            event_tx: event_tx.clone(),
            should_quit: Arc::clone(&should_quit),
            probe,
            host_geometry,
            initial_host_input: terminal_guard.take_buffered_host_input(),
            initial_host_geometry,
            reported_cell_size: Arc::clone(&reported_cell_size),
        });

        let write_stream = if let Some(attached) = initial {
            state
                .shell
                .endpoint_connected(&endpoint::ClientEndpointId::Local, local_generation);
            let mut registry = endpoint::EndpointRegistry::new_at(
                attached.activate(),
                local_generation,
                launch_now,
            );
            registry.send_to(
                &endpoint::ClientEndpointId::Local,
                &ClientMessage::ClientShellFocus {
                    focused: state.shell.host_focus_baseline(),
                },
            );
            registry
        } else {
            endpoint::EndpointRegistry::empty()
        };
        // The host helpers are running now and can latch a panic; stop before
        // setting up the remaining endpoints or drawing.
        if fatal.is_latched() {
            return Err(LoopExit::Panicked);
        }
        if local_failure_policy.reconnects_local() {
            // A connection or a failed handshake or setup occupies the first generation, and
            // a failure is recorded as its outcome so the retry follows the normal backoff. An
            // unreached socket used no generation, so the supervisor attempts at once.
            let generation = match &local_launch_state {
                LocalLaunchState::Attached | LocalLaunchState::Failed(_) => Some(local_generation),
                LocalLaunchState::Unreached => None,
            };
            supervisors.add_local(
                paths.server_address().socket().to_path_buf(),
                Arc::clone(&local_mismatch_guidance),
                generation,
                launch_now,
            );
            if let LocalLaunchState::Failed(failure) = &local_launch_state {
                let status = endpoint::EndpointFailureStatus::after_failure(failure);
                supervisors.record_status(
                    &endpoint::ClientEndpointId::Local,
                    local_generation,
                    status.into(),
                    launch_now,
                );
            }
        }
        match &local_launch_state {
            LocalLaunchState::Failed(failure) => {
                if failure.disposition().needs_attention() {
                    warn!(endpoint = "local", error = %failure, "endpoint needs attention");
                }
                state.present_notice(&shell::EndpointNotice::new(
                    endpoint::ClientEndpointId::Local,
                    shell::EndpointNoticeKind::StatusFailure(failure.to_string()),
                ));
            }
            LocalLaunchState::Unreached => state.mark_chrome_dirty(),
            LocalLaunchState::Attached => {}
        }
        state.present_pending();
        let hub = endpoint::EndpointHub::new(write_stream, supervisors, local_failure_policy);
        let client_loop = ClientLoop::new(
            state,
            hub,
            LoopSignals { should_quit, fatal },
            EventQueue {
                tx: event_tx,
                rx: event_rx,
            },
            HostCellReport {
                size: reported_cell_size,
                queried: cell_size_query,
            },
        );
        Ok(client_loop)
    }
}

// The local socket is always attempted before the event loop; there is no launch state where
// Local was deliberately omitted. A socket that refused the connection leaves no failure to
// report (the supervisor simply retries it), while a failed handshake or setup carries one.
enum LocalAtLaunch {
    Attached {
        endpoint: EndpointConnectionIo,
        generation: shepr_protocol::ConnectionGeneration,
    },
    Failed {
        failure: Option<shepr_launch::EndpointFailure>,
        generation: shepr_protocol::ConnectionGeneration,
    },
}

enum LocalLaunchState {
    Attached,
    /// The socket could not be reached; no attempt outcome is recorded, so the supervisor
    /// starts its first attempt at once.
    Unreached,
    Failed(shepr_launch::EndpointFailure),
}

/// What the host helpers start with: the stdin reader and the resize poller, both reading
/// the real host terminal and both sending on the loop's event queue.
struct HostHelpers {
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
    probe: input::HostInputProbe,
    host_geometry: SharedHostGeometry,
    /// Host input the keyboard probe read during terminal setup, replayed first.
    initial_host_input: Vec<u8>,
    /// The geometry the handshake sent, the resize poller's baseline.
    initial_host_geometry: HostGeometrySnapshot,
    reported_cell_size: Arc<AtomicCellSize>,
}

impl HostHelpers {
    /// Spawns the stdin reader and the resize poller threads. They run until `should_quit`
    /// is set or their terminal or queue goes; a panic in either is latched by the process
    /// panic hook (`fatal_panic`).
    fn spawn_threads(self) {
        let Self {
            event_tx,
            should_quit,
            probe,
            host_geometry,
            initial_host_input,
            initial_host_geometry,
            reported_cell_size,
        } = self;

        let stdin_tx = event_tx.clone();
        let stdin_quit = Arc::clone(&should_quit);
        let stdin_host_geometry = host_geometry.clone();
        std::thread::spawn(move || {
            input::stdin_reader_loop(
                &stdin_tx,
                &stdin_quit,
                &probe,
                &stdin_host_geometry,
                &initial_host_input,
            );
        });

        std::thread::spawn(move || {
            resize_poll_loop(
                &event_tx,
                initial_host_geometry,
                &reported_cell_size,
                &host_geometry,
                &should_quit,
            );
        });
    }
}

/// The run's result, read once after finalization. A latched panic outranks whatever the
/// launch returned, a loop exit or a launch failure included, and a launch that unwound
/// (`None`) is always one.
fn launch_outcome(
    launched: Option<Result<Result<(), LoopExit>, ClientRunError>>,
    fatal: &fatal_panic::FatalPanic,
) -> Result<ClientExit, ClientRunError> {
    let result = match launched {
        Some(Ok(result)) if !fatal.is_latched() => result,
        Some(Err(error)) if !fatal.is_latched() => return Err(error),
        _ => {
            let message = fatal
                .diagnostic()
                .unwrap_or("internal error: the client panicked")
                .to_owned();
            return Err(ClientRunError::Session(ClientExit::panicked(message)));
        }
    };
    let Err(err) = result else {
        return Ok(ClientExit::default());
    };
    let graceful_shutdown = matches!(&err, LoopExit::ServerShutdown { .. });
    let exit = ClientExit::from_loop(err);
    if graceful_shutdown {
        Ok(exit)
    } else {
        Err(ClientRunError::Session(exit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_fixtures::{AppPathsFixture as _, ValidatedClientConfigFixture as _};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    /// What `Launched::prepare` leaves when Local's socket could not be reached and a
    /// machine is configured, the one launch state that reaches the loop without a Local
    /// attach. Built directly: `prepare` reads the real terminal's size.
    fn launched_with_local_unreached(paths: &shepr_paths::AppPaths) -> Launched {
        let config = shepr_config::ValidatedClientConfig::test_from_config_with_paths(
            shepr_config::ClientConfig {
                machines: vec![shepr_config::MachineConfig {
                    label: shepr_config::MachineLabel::parse("remote").expect("test label"),
                    ssh: shepr_config::SshTarget::parse("remote").expect("test target"),
                }],
                ..Default::default()
            },
            None,
            paths.clone(),
        );
        let launch_now = std::time::Instant::now();
        // The SSH control socket's staging path does not fit under any directory in the
        // build tree, so the connectors take a runtime directory as short as a real
        // `/run/user/<uid>` that does not exist. These tests never connect: the missing
        // directory makes the connector's runtime directory check fail as a transient
        // `NotFound` before any path length is considered.
        let short_paths = shepr_paths::AppPaths::rooted_at(
            std::path::Path::new("/nonexistent/shepr-launch-tests"),
            None,
            None,
        )
        .expect("short test root");
        let connectors =
            endpoint::EndpointSupervisors::fresh_connectors(&short_paths, config.machines());
        let supervisors =
            endpoint::EndpointSupervisors::new(connectors, launch_now).expect("test supervisors");
        let machines = config.machines().to_vec();
        let initial_host_geometry = HostGeometrySnapshot {
            geometry: terminal_geometry::TerminalGeometry::new(
                shepr_core::geometry::GridSize::clamped(80, 24),
                shepr_core::geometry::HostCell::Unknown,
            ),
            pixel_extent: None,
        };
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel::<ClientLoopEvent>(CLIENT_EVENT_QUEUE_CAPACITY);
        Launched {
            launch_now,
            initial: LocalAtLaunch::Failed {
                failure: None,
                generation: endpoint::EndpointSupervisors::initial_local_generation(),
            },
            supervisors,
            local_failure_policy: endpoint::LocalFailurePolicy::for_machines(&machines),
            machines,
            initial_host_geometry,
            initial_geometry: terminal_geometry::bounded_cell_geometry(
                initial_host_geometry.geometry,
            ),
            event_tx,
            event_rx,
            settings: ClientSettings::resolve(&config).expect("test settings"),
            shell_config: shell::ClientShellConfig::from_validated_config(&config),
            paths: paths.clone(),
            local_mismatch_guidance: "test guidance".into(),
        }
    }

    /// A host helper that panics while the helpers start (here, the stand-in latching as
    /// the process panic hook would) stops the launch before the loop is built, and the
    /// run reports the panic, not a loop exit.
    #[test]
    fn a_helper_panic_latched_at_start_ends_the_launch_as_a_panic() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("launch-helper-panic");
        let paths = shepr_paths::AppPaths::test_at(&scratch);
        let fatal = Arc::new(fatal_panic::FatalPanic::default());
        let should_quit = Arc::new(AtomicBool::new(false));
        let (mut terminal_guard, output_writer) = TerminalGuard::detached();

        let helper_fatal = Arc::clone(&fatal);
        let built = launched_with_local_unreached(&paths).into_loop(
            output_writer,
            &mut terminal_guard,
            Arc::clone(&should_quit),
            Arc::clone(&fatal),
            move |_helpers| helper_fatal.latch(),
        );
        let Err(exit) = built else {
            panic!("a latched helper panic must stop the launch before the loop");
        };
        assert!(matches!(exit, LoopExit::Panicked));

        // The run's closure hands a loop exit from `into_loop` on as `Ok(Err(exit))`.
        let outcome = launch_outcome(Some(Ok(Err(exit))), &fatal);
        let Err(ClientRunError::Session(exit)) = outcome else {
            panic!("a latched panic is a failed session: {outcome:?}");
        };
        assert_eq!(
            exit.lines().collect::<Vec<_>>(),
            ["internal error: the client panicked"]
        );
    }

    fn every_loop_exit() -> Vec<LoopExit> {
        vec![
            LoopExit::HostTerminal(io::Error::new(io::ErrorKind::BrokenPipe, "host gone")),
            LoopExit::ServerShutdown {
                reason: shepr_protocol::ShutdownReason::Stopping,
            },
            LoopExit::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "peer reset")),
            LoopExit::Panicked,
        ]
    }

    fn session_lines(outcome: Result<ClientExit, ClientRunError>) -> Vec<String> {
        let Err(ClientRunError::Session(exit)) = outcome else {
            panic!("expected a failed session: {outcome:?}");
        };
        exit.lines().map(str::to_owned).collect()
    }

    /// Once a panic is latched, nothing the launch returned is reported: a clean loop
    /// end, every loop exit, a launch failure and an unwound launch all end as the panic.
    #[test]
    fn a_latched_panic_outranks_every_launch_result() {
        let fatal = fatal_panic::FatalPanic::default();
        fatal.latch();
        let panicked = ["internal error: the client panicked".to_owned()];

        assert_eq!(session_lines(launch_outcome(None, &fatal)), panicked);
        assert_eq!(
            session_lines(launch_outcome(Some(Ok(Ok(()))), &fatal)),
            panicked
        );
        for exit in every_loop_exit() {
            assert_eq!(
                session_lines(launch_outcome(Some(Ok(Err(exit))), &fatal)),
                panicked
            );
        }
        let launch_error = ClientRunError::Launch(io::Error::other("launch failed"));
        assert_eq!(
            session_lines(launch_outcome(Some(Err(launch_error)), &fatal)),
            panicked
        );
    }

    #[test]
    fn an_unlatched_launch_reports_what_it_returned() {
        let fatal = fatal_panic::FatalPanic::default();

        let clean = launch_outcome(Some(Ok(Ok(()))), &fatal).expect("a clean end succeeds");
        assert_eq!(clean.lines().count(), 0);

        let launch_error = ClientRunError::Launch(io::Error::other("launch failed"));
        let outcome = launch_outcome(Some(Err(launch_error)), &fatal);
        let Err(ClientRunError::Launch(error)) = outcome else {
            panic!("a launch failure stays one: {outcome:?}");
        };
        assert_eq!(error.to_string(), "launch failed");
        assert!(!fatal.is_latched());
    }

    /// A server that shut down ends the session successfully, still saying why; every
    /// other loop exit is a failed session whose line is that exit.
    #[test]
    fn loop_exits_map_to_their_reported_outcome() {
        let fatal = fatal_panic::FatalPanic::default();
        for exit in every_loop_exit() {
            let expected = exit.to_string();
            let graceful = matches!(&exit, LoopExit::ServerShutdown { .. });
            let outcome = launch_outcome(Some(Ok(Err(exit))), &fatal);
            if graceful {
                let exit = outcome.expect("a server shutdown is a successful exit");
                assert_eq!(exit.lines().collect::<Vec<_>>(), [expected.as_str()]);
            } else {
                assert_eq!(session_lines(outcome), [expected]);
            }
        }
    }

    /// The control: helpers that start cleanly leave a loop to run, so the latch is what
    /// stopped the launch above.
    #[test]
    fn helpers_that_start_cleanly_leave_a_loop_to_run() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("launch-helper-start");
        let paths = shepr_paths::AppPaths::test_at(&scratch);
        let fatal = Arc::new(fatal_panic::FatalPanic::default());
        let (mut terminal_guard, output_writer) = TerminalGuard::detached();

        let mut started = 0;
        let built = launched_with_local_unreached(&paths).into_loop(
            output_writer,
            &mut terminal_guard,
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&fatal),
            |_helpers| started += 1,
        );
        assert!(built.is_ok(), "an unlatched launch builds its loop");
        assert_eq!(started, 1, "the helpers start once");
        assert!(!fatal.is_latched());
    }

    #[test]
    fn impossible_connector_paths_fail_in_the_preterminal_phase() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("launch-admission");
        // Size the root so `root/runtime/shepr.sock` is exactly the longest
        // path Linux accepts: the server socket fits, and any SSH bridge
        // socket name beside it (longer than `shepr.sock`) cannot.
        let server_socket_tail = "/runtime/shepr.sock";
        let scratch_len = scratch.as_os_str().as_bytes().len();
        let root_len = shepr_core::socket_path::UNIX_SOCKET_PATH_MAX - server_socket_tail.len();
        let padding = root_len
            .checked_sub(scratch_len + 1)
            .filter(|padding| *padding > 0)
            .expect("scratch directory leaves room for a root component");
        let root = scratch.join("x".repeat(padding));
        std::fs::create_dir_all(root.join("runtime")).expect("test runtime directory");
        for dir in [root.clone(), root.join("runtime")] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .expect("test private runtime directory");
        }
        assert!(
            shepr_paths::AppPaths::rooted_at(&root, None, None).is_ok(),
            "the server socket path fits"
        );
        assert!(
            shepr_remote::validate_remote_bridge_endpoint_path(
                &root.join("runtime"),
                "bridge.sock",
                "b.sock"
            )
            .is_err(),
            "bridge socket paths do not fit"
        );
        let paths = shepr_paths::AppPaths::test_at(&root);
        let config = shepr_config::ValidatedClientConfig::test_from_config_with_paths(
            shepr_config::ClientConfig {
                machines: vec![shepr_config::MachineConfig {
                    label: shepr_config::MachineLabel::parse("remote").expect("test label"),
                    ssh: shepr_config::SshTarget::parse("remote").expect("test target"),
                }],
                ..Default::default()
            },
            None,
            paths.clone(),
        );
        let connectors = endpoint::EndpointSupervisors::fresh_connectors(&paths, config.machines());
        let error = Launched::prepare(&config, &paths, connectors)
            .err()
            .expect("connector admission fails");
        assert!(matches!(&error, ClientRunError::Launch(_)));
        assert!(
            error.to_string().contains("shorten XDG_RUNTIME_DIR"),
            "{error}"
        );
    }
}
