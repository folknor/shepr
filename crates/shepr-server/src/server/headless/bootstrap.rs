use super::*;
use crate::limits::{API_REQUEST_CHANNEL_CAPACITY, TOKIO_RUNTIME_SHUTDOWN_TIMEOUT};

/// Why [`run_server`] refused to start or stopped with an error. The server
/// prints nothing itself: the binary renders this and picks the exit status.
#[derive(Debug)]
pub enum RunServerError {
    /// Another server already listens on `path`.
    AlreadyRunning {
        path: PathBuf,
    },
    /// Another server already holds the lease on this profile's data
    /// directory, the canonical `directory`. The lease is taken before the
    /// socket is bound.
    DataDirHeld {
        directory: PathBuf,
    },
    SessionTarget(io::Error),
    PaneLaunch(io::Error),
    Socket(io::Error),
    Runtime(io::Error),
    Lease(io::Error),
    Logging(io::Error),
    /// The termination signal handler could not be installed, so the loop
    /// never started.
    SignalInstall(io::Error),
    /// Shutdown completion was asked for from a lifecycle phase it does not
    /// start from.
    Shutdown(UnexpectedPhase),
}

impl std::fmt::Display for RunServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { path } => write!(
                f,
                "another server listens on the socket ({})",
                path.display()
            ),
            Self::DataDirHeld { directory } => write!(
                f,
                "another server holds the data directory {}",
                directory.display()
            ),
            Self::SessionTarget(error)
            | Self::PaneLaunch(error)
            | Self::Socket(error)
            | Self::Runtime(error)
            | Self::Lease(error)
            | Self::Logging(error)
            | Self::SignalInstall(error) => error.fmt(f),
            Self::Shutdown(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RunServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SessionTarget(error)
            | Self::PaneLaunch(error)
            | Self::Socket(error)
            | Self::Runtime(error)
            | Self::Lease(error)
            | Self::Logging(error)
            | Self::SignalInstall(error) => Some(error),
            Self::Shutdown(error) => Some(error),
            Self::AlreadyRunning { .. } | Self::DataDirHeld { .. } => None,
        }
    }
}

/// Where a started server listens and logs, handed to the `on_ready` callback
/// of [`run_server`] once the socket is bound and the TUI gate is open. Its
/// `Display` form is the start of the operator notice a foreground server
/// shows; the `shepr-server` executable adds the client command to run, from
/// `shepr_launch::guidance`, which this crate does not link.
#[derive(Clone, Debug)]
pub struct ServerReady {
    pub socket: PathBuf,
    pub log_file: PathBuf,
    /// Why the server runs without file logging, when `log_file` could not be
    /// opened at startup.
    pub log_file_unavailable: Option<shepr_platform::logging::FileLoggingUnavailable>,
}

impl std::fmt::Display for ServerReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "the shepr server is running; you can use any shepr CLI command in another terminal."
        )?;
        writeln!(f, "socket: {}", self.socket.display())?;
        match &self.log_file_unavailable {
            None => write!(f, "logs: {}", self.log_file.display()),
            Some(unavailable) => write!(
                f,
                "logs: unavailable, could not open {}: {}",
                unavailable.path.display(),
                unavailable.reason
            ),
        }
    }
}

/// Runs the headless server: [`start_server`] takes it from the
/// data-directory lease to an open TUI gate, then `on_ready` reports it and
/// the loop runs. Shutdown keeps the socket through the final save, retires
/// the lease, then removes the socket
/// (`HeadlessServer::release_socket_after_save`).
///
/// `on_ready` runs once, after the TUI gate is open and before the event
/// loop starts; the binary uses it to tell a foreground operator where the
/// server listens. It runs on the tokio runtime, so it must not block.
pub fn run_server(
    config: &shepr_config::ValidatedServerConfig,
    paths: &shepr_paths::AppPaths,
    on_ready: impl FnOnce(&ServerReady),
) -> Result<(), RunServerError> {
    let StartedServer {
        runtime: rt,
        mut server,
        ready,
    } = start_server(config, paths, start_file_logging, |step| {
        debug!(?step, "server startup step done");
    })?;

    let result = rt.block_on(async move {
        shepr_platform::structured_log!(INFO, event = server.startup, outcome = Ok, socket = %ready.socket.display(), "shepr server started");
        on_ready(&ready);

        server.run().await.map_err(|error| {
            // A client-spawned server's stderr is /dev/null by now.
            shepr_platform::structured_log!(ERROR, event = server.event_loop, outcome = Error, %error, "the server event loop failed");
            error
        })
    });

    rt.shutdown_timeout(TOKIO_RUNTIME_SHUTDOWN_TIMEOUT);
    crate::logging::shutdown();
    result
}

/// A step of [`start_server`], reported to its observer once done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupStep {
    /// The data-directory lease is held and the session path accepted.
    LeaseHeld,
    /// The socket is bound; `ping` answers `starting` from now on.
    SocketBound,
    /// The saved session is restored (or the startup workspace seeded).
    PanesRestored,
    /// The TUI gate is open; `ping` no longer answers `starting`.
    ClientProtocolOpen,
}

/// A server [`start_server`] brought up to an open TUI gate, its loop not
/// yet running. The runtime holds the tasks the restore spawned.
struct StartedServer {
    runtime: tokio::runtime::Runtime,
    server: HeadlessServer,
    ready: ServerReady,
}

/// The startup sequence of [`run_server`], in this order: take the
/// data-directory lease; refuse a session path no save could replace; start
/// file logging (`start_logging`, the one process-wide step, which only a
/// server process may take), the detection manifests, the pane launch service
/// and the integration installer; bind the socket, which answers `ping` as
/// `starting` from then on; build the runtime; restore panes; build
/// [`HeadlessServer`]; open the TUI gate. `on_step` hears each
/// [`StartupStep`] as it completes.
fn start_server(
    config: &shepr_config::ValidatedServerConfig,
    paths: &shepr_paths::AppPaths,
    start_logging: impl FnOnce(
        &std::path::Path,
    )
        -> Result<shepr_platform::logging::FileLoggingOutcome, RunServerError>,
    mut on_step: impl FnMut(StartupStep),
) -> Result<StartedServer, RunServerError> {
    shepr_mux::pane::init_osc_evidence_capture().map_err(RunServerError::PaneLaunch)?;
    let socket = paths.server_address().socket().to_path_buf();

    // AppPaths validated and retained the one-time startup handoff. Pane
    // launches scrub it from their child environments.
    let startup_cwd = paths.startup_cwd().cloned();

    let data_dir = paths.data_dir();

    let (api_tx, api_rx) = tokio::sync::mpsc::channel(API_REQUEST_CHANNEL_CAPACITY);
    let stop_signal = Arc::new(shepr_api::ServerStopSignal::default());
    // The one boot identity of this server lifetime: the listener's `ping`
    // and stop guard and the client shell lane all report this value.
    let boot_id = mint_boot_id(super::sample_app_clock().wall_now);

    // Field order releases the lease before the socket on startup failure.
    struct Reserved {
        lease: shepr_mux::persist::DataDirLease,
        api: shepr_api::ServerHandle,
        file_logging: shepr_platform::logging::FileLoggingOutcome,
    }
    let lease = shepr_mux::persist::DataDirLease::acquire(data_dir).map_err(lease_error)?;
    // A session path no save can replace (a directory, a FIFO) refuses the
    // start before anything is restored or launched, rather than running
    // panes whose layout can never be saved.
    shepr_mux::persist::check_session_target(&lease).map_err(RunServerError::SessionTarget)?;
    on_step(StartupStep::LeaseHeld);
    let server_log = paths.server_log();
    let file_logging = start_logging(&server_log)?;
    // Compile the bundled detection manifests off the tokio loop, before App
    // restores PTYs whose detection workers consult them, and after logging
    // starts, so a bundled manifest that fails to compile reaches the log.
    shepr_detect::manifest::compile_bundled_manifests();
    // Everything a pane launch would otherwise do on its first spawn that may
    // block (the launch status listener, the passwd lookup, resolving this
    // binary's path), done before any pane is restored or created.
    shepr_mux::pane::init_pane_launches().map_err(RunServerError::PaneLaunch)?;
    spawn_integration_install();
    let api = shepr_api::start_server(api_tx, Arc::clone(&stop_signal), paths, boot_id.clone())
        .map_err(startup_error)?;
    on_step(StartupStep::SocketBound);
    let reserved = Reserved {
        lease,
        api,
        file_logging,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            // The socket is bound, so a stop may already be waiting on the
            // final save result; a failed start owes it an explicit answer,
            // written before `reserved` drops and the socket goes.
            if stop_signal
                .complete_unfinished_final_save(super::lifecycle::UNFINISHED_FINAL_SAVE_MESSAGE)
            {
                stop_signal.wait_for_stop_answers(crate::limits::STOP_ANSWER_WAIT);
            }
            RunServerError::Runtime(error)
        })?;

    // The restore spawns pane tasks, so it runs inside the runtime.
    let (server, ready) = runtime.block_on(async move {
        let Reserved {
            lease,
            api,
            file_logging,
        } = reserved;
        let (mut app, outputs) = app::App::open(config, paths, lease, super::sample_app_clock());
        seed_startup_workspace_if_empty(&mut app, startup_cwd);
        on_step(StartupStep::PanesRestored);
        let server = HeadlessServer::new(app, outputs, api_rx, api, stop_signal, boot_id);
        server.open_client_protocol();
        on_step(StartupStep::ClientProtocolOpen);
        let ready = ServerReady {
            socket,
            log_file: paths.server_log(),
            log_file_unavailable: file_logging.unavailable,
        };
        (server, ready)
    });
    Ok(StartedServer {
        runtime,
        server,
        ready,
    })
}

/// Starts this server process's file logging at `server_log` and, when the
/// log file opened, routes panics to it. A log file that cannot be opened
/// does not stop the server; the ready notice says so instead of naming a log
/// that is not being written.
fn start_file_logging(
    server_log: &std::path::Path,
) -> Result<shepr_platform::logging::FileLoggingOutcome, RunServerError> {
    let file_logging =
        shepr_platform::logging::init_file_logging(server_log).map_err(RunServerError::Logging)?;
    if file_logging.unavailable.is_none() {
        log_panics();
    }
    Ok(file_logging)
}

/// The boot identity of a server lifetime starting at `wall_now` in this
/// process. The launcher reads the server's pid back out of it, and its clock
/// part tells this lifetime from an earlier server that had the same pid.
fn mint_boot_id(wall_now: std::time::SystemTime) -> shepr_protocol::BootId {
    let since_epoch = match wall_now.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => Ok(duration),
        Err(error) => Err(error.duration()),
    };
    shepr_protocol::BootId::from_process_clock(std::process::id(), since_epoch)
}

/// Makes every panic reach the server log through `tracing`, then runs the
/// previous hook (the default one prints to stderr). A client-spawned server
/// points its stderr at `/dev/null` once it is ready, so the log is the only
/// place a later panic is recorded.
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        shepr_platform::structured_log!(
            ERROR,
            event = server.panic,
            outcome = Panicked,
            thread = thread.name().unwrap_or("<unnamed>"),
            "{info}"
        );
        previous(info);
    }));
}

/// Installs or updates the agent hooks on this host in a detached thread, so
/// the file IO stays off the startup path. The
/// agent config locations are read from the environment here, before the
/// thread starts. Every outcome goes to the log; nothing here can fail the
/// launch. A server that stops while the thread runs leaves at most a
/// half-finished install, which the next launch completes: every file is
/// replaced by rename, never rewritten in place. Agent configs are shared by
/// every build on the host and only release hooks are installed into them, so
/// this server's compiled profile decides, before agent paths are resolved or
/// any file IO starts; the inherited pane marker does not.
fn spawn_integration_install() {
    if !integration_install_enabled(shepr_paths::BuildProfile::current()) {
        shepr_platform::structured_log!(
            INFO,
            event = integration.install,
            outcome = Skipped,
            "agent integration installation skipped; only release servers own agent configs"
        );
        return;
    }
    let paths = shepr_integration::AgentIntegrationPaths::resolve();
    if let Err(error) = std::thread::Builder::new()
        // Linux exposes at most 15 bytes through `pthread_setname_np`.
        .name("agent-install".into())
        .spawn(move || {
            shepr_integration::install_present_integrations(&paths);
        })
    {
        shepr_platform::structured_log!(WARN, event = integration.worker_start, outcome = Error, %error, "could not start the agent integration install");
    }
}

fn integration_install_enabled(profile: shepr_paths::BuildProfile) -> bool {
    profile == shepr_paths::BuildProfile::Release
}

fn seed_startup_workspace_if_empty(
    app: &mut app::App,
    startup_cwd: Option<shepr_core::absolute_path::AbsolutePath>,
) {
    let Some(cwd) = startup_cwd else {
        return;
    };

    if !app.state().workspaces().is_empty() {
        shepr_platform::structured_log!(
            INFO, event = workspace.startup, outcome = Skipped,
            cwd = %cwd.display(),
            "restored session already has workspaces; ignoring startup cwd"
        );
        return;
    }

    // No client has attached yet, so the workspace is sized for the headless
    // area; the first client's geometry pass resizes it.
    let geometry = app.headless_spawn_geometry();
    match app.create_workspace(&cwd, geometry) {
        Ok(_) => {
            shepr_platform::structured_log!(INFO, event = workspace.startup, outcome = Ok, cwd = %cwd.display(), "created startup workspace");
        }
        Err(err) => {
            shepr_platform::structured_log!(WARN, event = workspace.startup, outcome = Error, cwd = %cwd.display(), error = %err, "failed to create startup workspace");
        }
    }
}

/// Classifies a socket bind failure. A platform busy refusal means another
/// server owns the path; any other error, including an unrelated `AddrInUse`,
/// stays an IO failure. The refusal is recorded in the server log as well: a
/// daemonized server's stderr goes nowhere.
fn startup_error(error: shepr_platform::ipc::BindError) -> RunServerError {
    match error {
        shepr_platform::ipc::BindError::Busy(busy) => {
            let path = busy.path().to_path_buf();
            shepr_platform::structured_log!(ERROR, event = ipc.socket_bind, outcome = Busy, path = %path.display(), "another server already listens on the socket");
            RunServerError::AlreadyRunning { path }
        }
        shepr_platform::ipc::BindError::Io(error) => RunServerError::Socket(error),
    }
}

/// Classifies a failure taking the data-directory lease. Only the
/// held-lease refusal from `shepr_mux::persist` means a server is already
/// running; any other error stays an IO failure. Unlike [`startup_error`] this
/// is not logged: file logging starts only once the lease is held, and the log
/// file lives in the directory the other server owns.
fn lease_error(error: shepr_platform::LeaseAcquireError) -> RunServerError {
    match error {
        shepr_platform::LeaseAcquireError::Held(held) => RunServerError::DataDirHeld {
            directory: held.directory().to_path_buf(),
        },
        shepr_platform::LeaseAcquireError::Io(error) => RunServerError::Lease(error),
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn only_release_servers_install_shared_agent_integrations() {
        assert!(integration_install_enabled(
            shepr_paths::BuildProfile::Release
        ));
        assert!(!integration_install_enabled(shepr_paths::BuildProfile::Dev));
    }

    /// Drives `run_server`'s own startup sequence, checking at every step
    /// what the outside world sees: the lease before the socket, the socket
    /// answering `starting` through the restore, the gate open only after it,
    /// and one boot identity for the listener and the server. Only file
    /// logging is stood in for, since it is process-wide.
    #[test]
    fn startup_binds_after_the_lease_and_opens_the_gate_after_restore() {
        use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};
        use std::cell::RefCell;
        use std::io::Write;
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("bootstrap-gate");
        let paths = shepr_paths::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
            shepr_config::ServerConfig::default(),
            paths.clone(),
        );
        let client = shepr_api::client::ApiClient::for_socket(paths.server_address().socket());
        let observed = RefCell::new(Vec::new());
        let lease_held_and_unbound = |when: &str| {
            assert!(
                matches!(
                    shepr_mux::persist::DataDirLease::acquire(paths.data_dir()),
                    Err(shepr_platform::LeaseAcquireError::Held(_))
                ),
                "{when}: the data-directory lease is held"
            );
            assert!(
                client.ping().is_err(),
                "{when}: the socket is not bound yet"
            );
        };

        let started = start_server(
            &config,
            &paths,
            |server_log| {
                assert_eq!(server_log, paths.server_log().as_path());
                lease_held_and_unbound("file logging");
                observed.borrow_mut().push("FileLogging".to_owned());
                Ok(shepr_platform::logging::FileLoggingOutcome { unavailable: None })
            },
            |step| {
                match step {
                    StartupStep::LeaseHeld => lease_held_and_unbound("lease held"),
                    StartupStep::SocketBound | StartupStep::PanesRestored => assert!(
                        client.ping().expect("ping after the bind").starting,
                        "{step:?}: the gate stays closed until panes are restored"
                    ),
                    StartupStep::ClientProtocolOpen => {
                        let open = client.ping().expect("ping after the gate");
                        assert!(
                            !open.starting && !open.stopping,
                            "{step:?}: the gate is open"
                        );
                    }
                }
                observed.borrow_mut().push(format!("{step:?}"));
            },
        )
        .expect("the server starts");
        assert_eq!(
            observed.into_inner(),
            [
                "LeaseHeld",
                "FileLogging",
                "SocketBound",
                "PanesRestored",
                "ClientProtocolOpen"
            ]
        );
        let StartedServer {
            runtime,
            server,
            ready,
        } = started;
        assert_eq!(ready.socket, paths.server_address().socket());
        assert!(ready.log_file_unavailable.is_none());

        let pong = client.ping().expect("ready pong");
        assert!(!pong.starting && !pong.stopping);
        assert_eq!(
            pong.boot_id, server.client_shell_boot_id,
            "ping reports the boot the server lifetime was built with"
        );
        let mut peer = shepr_platform::ipc::connect_local_stream(paths.server_address().socket())
            .expect("TUI connect");
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .expect("deadline");
        peer.write_all(&shepr_protocol::preamble::local_preamble())
            .expect("identity");
        shepr_protocol::write_message(
            &mut peer,
            &shepr_protocol::ClientMessage::EndpointHello(
                shepr_protocol::endpoint::EndpointClientHello {
                    geometry: shepr_protocol::TerminalGeometry::from_host(
                        shepr_core::geometry::GridSize::clamped(80, 24),
                        shepr_core::geometry::HostCell::from_host(8, 16, true),
                    ),
                    mouse_capture: true,
                    surface_active: true,
                },
            ),
        )
        .expect("hello");
        shepr_protocol::preamble::read_preamble(&mut peer).expect("server identity");
        let welcome: shepr_protocol::ServerMessage =
            shepr_protocol::read_message(&mut peer).expect("welcome");
        assert_eq!(
            welcome,
            shepr_protocol::ServerMessage::EndpointWelcome(
                shepr_protocol::endpoint::EndpointServerWelcome::Accepted
            )
        );
        drop(peer);
        {
            // The app's pane tasks belong to the runtime the restore ran on.
            let _context = runtime.enter();
            drop(server);
        }
        drop(runtime);
        assert!(
            !paths
                .server_address()
                .socket()
                .try_exists()
                .expect("stat the socket"),
            "dropping the server removes its socket"
        );
    }
}
