use super::*;

/// Which of the server's two sockets another server already holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerSocket {
    /// The JSON API socket.
    Api,
    /// The binary client-protocol socket.
    Client,
}

impl std::fmt::Display for ServerSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Api => "api socket",
            Self::Client => "client socket",
        })
    }
}

/// Why [`run_server`] refused to start or stopped with an error. The server
/// prints nothing itself: the binary renders this and picks the exit status.
#[derive(Debug)]
pub enum RunServerError {
    /// Another server already listens on `path`.
    AlreadyRunning { socket: ServerSocket, path: PathBuf },
    /// Another server already holds the lease on this profile's data
    /// directory, the canonical `directory`. The lease is taken before either
    /// socket is bound.
    DataDirHeld { directory: PathBuf },
    /// Startup or the event loop failed.
    Io(io::Error),
}

impl std::fmt::Display for RunServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { socket, path } => write!(
                f,
                "another server listens on the {socket} ({})",
                path.display()
            ),
            Self::DataDirHeld { directory } => write!(
                f,
                "another server holds the data directory {}",
                directory.display()
            ),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RunServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::AlreadyRunning { .. } | Self::DataDirHeld { .. } => None,
        }
    }
}

impl From<io::Error> for RunServerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Where a started server listens and logs, handed to the `on_ready` callback
/// of [`run_server`] once both sockets are bound. Its `Display` form is the
/// operator notice a foreground server shows.
#[derive(Clone, Debug)]
pub struct ServerReady {
    pub api_socket: PathBuf,
    pub client_socket: PathBuf,
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
        writeln!(f, "api socket: {}", self.api_socket.display())?;
        writeln!(f, "client socket: {}", self.client_socket.display())?;
        match &self.log_file_unavailable {
            None => writeln!(f, "logs: {}", self.log_file.display())?,
            Some(unavailable) => writeln!(
                f,
                "logs: unavailable, could not open {}: {}",
                unavailable.path.display(),
                unavailable.reason
            )?,
        }
        write!(
            f,
            "did you mean to open the Shepr TUI? run `shepr`, which starts the server itself."
        )
    }
}

/// Run the headless server. This is the entry point called from main.rs.
///
/// `on_ready` runs once, after both sockets are bound and before the event
/// loop starts; the binary uses it to tell a foreground operator where the
/// server listens. It runs on the tokio runtime, so it must not block.
pub fn run_server(
    config: &shepr_config::ValidatedServerConfig,
    paths: &shepr_config::AppPaths,
    on_ready: impl FnOnce(&ServerReady),
) -> Result<(), RunServerError> {
    let api_socket = shepr_api::socket_path(paths);
    let client_socket = client_socket_path(paths);

    // The startup-cwd hint stays in this process's environment; every child
    // launch path scrubs it instead of the server unsetting it here.
    let startup_cwd = read_startup_cwd();

    let data_dir = paths.data_dir();

    let (api_tx, api_rx) = tokio::sync::mpsc::channel(crate::limits::API_REQUEST_CHANNEL_CAPACITY);
    let stop_requested = Arc::new(shepr_api::ServerStopSignal::default());

    let reserved = shepr_platform::ipc::ServerLifetime::reserve(
        || shepr_mux::persist::DataDirLease::acquire(data_dir).map_err(lease_error),
        |_| {
            // A log file that cannot be opened does not stop the server; the ready
            // notice says so instead of naming a log that is not being written.
            let file_logging = shepr_platform::logging::init_file_logging(
                data_dir,
                shepr_platform::logging::SERVER_LOG_FILE,
            )?;
            if file_logging.unavailable.is_none() {
                log_panics();
            }
            // Compile the bundled detection manifests off the tokio loop, before App
            // restores PTYs whose detection workers consult them, and after logging
            // starts, so a bundled manifest that fails to compile reaches the log.
            shepr_agent::detect::manifest::compile_bundled_manifests();
            spawn_integration_install();

            let api = shepr_api::start_server(api_tx.clone(), Arc::clone(&stop_requested), paths)
                .map_err(|error| startup_error(ServerSocket::Api, error))?;
            Ok((api, file_logging))
        },
        || {
            reserve_client_socket_startup_lock(&client_socket)
                .map_err(|error| startup_error(ServerSocket::Client, error))
        },
    )?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async move {
        let (mut server, file_logging) = reserved.restore_and_bind(
            |lease| {
                let mut app = app::App::with_paths(
                    config,
                    paths,
                    lease,
                    app::AppPolicy::Production,
                    super::sample_app_clock(),
                );
                seed_startup_workspace_if_empty(&mut app, startup_cwd);
                app
            },
            |app, (api, file_logging), reservation| {
                HeadlessServer::new(app, api_rx, Some(api), stop_requested, reservation)
                    .map(|server| (server, file_logging))
                    .map_err(|error| startup_error(ServerSocket::Client, error))
            },
        )?;

        let ready = ServerReady {
            api_socket,
            client_socket,
            log_file: data_dir.join(shepr_platform::logging::SERVER_LOG_FILE),
            log_file_unavailable: file_logging.unavailable,
        };
        info!(
            api_socket = %ready.api_socket.display(),
            client_socket = %ready.client_socket.display(),
            "shepr server started"
        );
        on_ready(&ready);

        server.run().await.map_err(|error| {
            // A client-spawned server's stderr is /dev/null by now.
            tracing::error!(%error, "the server event loop failed");
            RunServerError::from(error)
        })
    });

    rt.shutdown_timeout(crate::limits::TOKIO_RUNTIME_SHUTDOWN_TIMEOUT);
    crate::logging::shutdown("server");
    result
}

fn reserve_client_socket_startup_lock(path: &std::path::Path) -> io::Result<SocketStartupLock> {
    let startup_lock = shepr_platform::ipc::acquire_socket_startup_lock(path)?;
    if shepr_platform::ipc::ServerLifetime::endpoint_is_live(path)? {
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            ClientSocketAlreadyLive {
                path: path.to_path_buf(),
            },
        ))
    } else {
        Ok(startup_lock)
    }
}

#[derive(Debug)]
struct ClientSocketAlreadyLive {
    path: PathBuf,
}

impl std::fmt::Display for ClientSocketAlreadyLive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "client socket is already live at {}",
            self.path.display()
        )
    }
}

impl std::error::Error for ClientSocketAlreadyLive {}

/// Makes every panic reach the server log through `tracing`, then runs the
/// previous hook (the default one prints to stderr). A client-spawned server
/// points its stderr at `/dev/null` once it is ready, so the log is the only
/// place a later panic is recorded.
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        tracing::error!(thread = thread.name().unwrap_or("<unnamed>"), "{info}");
        previous(info);
    }));
}

/// Installs or updates the agent hooks on this host in a detached thread, so
/// the file IO stays off the startup path. The
/// agent config locations are read from the environment here, before the
/// thread starts. Every outcome goes to the log; nothing here can fail the
/// launch. A server that stops while the thread runs leaves at most a
/// half-finished install, which the next launch completes: every file is
/// replaced by rename, never rewritten in place. The installer is given this
/// build's compiled profile, never the inherited pane marker, and skips
/// everything unless it is release: agent configs are shared by every build on
/// the host, and only release hooks are installed into them.
fn spawn_integration_install() {
    let paths = shepr_agent::integration::AgentIntegrationPaths::resolve();
    let build_profile = shepr_config::BuildProfile::current().marker();
    if let Err(error) = std::thread::Builder::new()
        .name("integration-install".into())
        .spawn(move || {
            shepr_agent::integration::install_present_integrations(&paths, build_profile);
        })
    {
        warn!(%error, "could not start the agent integration install");
    }
}

fn seed_startup_workspace_if_empty(app: &mut app::App, startup_cwd: Option<PathBuf>) {
    let Some(cwd) = startup_cwd else {
        return;
    };

    if !app.state.workspaces.is_empty() {
        info!(
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
            info!(cwd = %cwd.display(), "created startup workspace");
        }
        Err(err) => {
            warn!(cwd = %cwd.display(), error = %err, "failed to create startup workspace");
        }
    }
}

/// Read the startup-cwd hint the spawning client left in the environment.
/// Pane launches and `shepr_platform::child_command` remove the handoff
/// variable from their child environments.
fn read_startup_cwd() -> Option<PathBuf> {
    let var = shepr_core::env::EnvVar::SheprStartupCwd;
    startup_cwd_from_env_value(shepr_core::env::read_path(var))
}

/// The startup cwd a handoff read produced. The variable is carried byte for
/// byte, so only empty (unset) is ever absent; a refusal cannot happen for this
/// kind, and would only mean no startup workspace.
fn startup_cwd_from_env_value(
    value: Result<Option<PathBuf>, shepr_core::env::EnvError>,
) -> Option<PathBuf> {
    value.unwrap_or_else(|error| {
        warn!(%error, "ignoring the startup directory hint");
        None
    })
}

/// Classifies a socket failure. A platform busy refusal, or a live client
/// listener found during preflight, means another server owns that path; any
/// other error, including an unrelated `AddrInUse`, stays an IO failure. The
/// refusal is recorded in the server log as well: a daemonized server's stderr
/// goes nowhere.
fn startup_error(socket: ServerSocket, error: io::Error) -> RunServerError {
    let path = shepr_platform::ipc::SocketBusy::from_io(&error)
        .map(|busy| busy.path().to_path_buf())
        .or_else(|| {
            if socket != ServerSocket::Client {
                return None;
            }
            error
                .get_ref()?
                .downcast_ref::<ClientSocketAlreadyLive>()
                .map(|busy| busy.path.clone())
        });
    let Some(path) = path else {
        return RunServerError::Io(error);
    };
    tracing::error!(%socket, path = %path.display(), "another server already listens on the socket");
    RunServerError::AlreadyRunning { socket, path }
}

/// Classifies a failure taking the data-directory lease. Only the
/// held-lease refusal from `shepr_mux::persist` means a server is already
/// running; any other error stays an IO failure. Unlike [`startup_error`] this
/// is not logged: file logging starts only once the lease is held, and the log
/// file lives in the directory the other server owns.
fn lease_error(error: io::Error) -> RunServerError {
    let Some(held) = shepr_mux::persist::DataDirLeaseHeld::from_io(&error) else {
        return RunServerError::Io(error);
    };
    RunServerError::DataDirHeld {
        directory: held.directory().to_path_buf(),
    }
}

#[cfg(test)]
mod client_socket_reservation_and_startup_cwd_tests {
    use super::*;

    #[test]
    fn client_socket_reservation_refuses_an_existing_server_before_restore() {
        let scratch = shepr_test_support::ScratchDir::new("client-socket-reservation");
        let path = scratch.join("client.sock");
        let (_listener, _lock, _identity) =
            shepr_platform::ipc::bind_private_socket(&path).expect("hold client socket");

        let error = match reserve_client_socket_startup_lock(&path) {
            Ok(_) => panic!("a running server owns the client startup lock"),
            Err(error) => error,
        };

        let busy = shepr_platform::ipc::SocketBusy::from_io(&error)
            .expect("reservation preserves the busy socket error");
        assert_eq!(busy.path(), path.as_path());
    }

    #[test]
    fn client_socket_reservation_refuses_a_live_listener_without_its_lock() {
        let scratch = shepr_test_support::ScratchDir::new("unlocked-client-socket");
        let path = scratch.join("client.sock");
        let (listener, lock, _identity) =
            shepr_platform::ipc::bind_private_socket(&path).expect("hold client socket");
        drop(lock);

        let error = match reserve_client_socket_startup_lock(&path) {
            Ok(_) => panic!("a live listener makes the client socket busy"),
            Err(error) => error,
        };

        assert!(matches!(
            startup_error(ServerSocket::Client, error),
            RunServerError::AlreadyRunning { socket: ServerSocket::Client, path: found }
                if found == path
        ));
        drop(listener);
    }

    #[test]
    fn client_socket_reservation_does_not_publish_its_listener_path() {
        let scratch = shepr_test_support::ScratchDir::new("unpublished-client-socket");
        let path = scratch.join("client.sock");

        let startup_lock =
            reserve_client_socket_startup_lock(&path).expect("reserve an unused client socket");

        assert!(
            !path.try_exists().expect("stat the client socket path"),
            "the launcher must keep seeing API-first startup"
        );
        drop(startup_lock);
    }

    #[test]
    fn client_socket_reservation_can_be_consumed_by_the_platform_binder() {
        let scratch = shepr_test_support::ScratchDir::new("client-reservation-bind");
        let path = scratch.join("client.sock");
        let reservation = reserve_client_socket_startup_lock(&path).expect("reserve socket");
        let (_listener, _lock, _identity) =
            shepr_platform::ipc::bind_private_socket_with_lock(reservation)
                .expect("bind reserved socket");
        let error = reserve_client_socket_startup_lock(&path)
            .err()
            .expect("still locked");
        let busy = shepr_platform::ipc::SocketBusy::from_io(&error).expect("busy");
        assert_eq!(busy.path(), path.as_path());
    }

    #[tokio::test]
    async fn bootstrap_reservation_binds_the_resolved_client_socket() {
        use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("bootstrap-client-path");
        let paths = shepr_config::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
            shepr_config::ServerConfig::default(),
            paths.clone(),
        );
        let expected = client_socket_path(&paths);
        let (api_tx, api_rx) = mpsc::channel(crate::limits::API_REQUEST_CHANNEL_CAPACITY);
        let stop = Arc::new(shepr_api::ServerStopSignal::default());
        let reserved = shepr_platform::ipc::ServerLifetime::reserve(
            || shepr_mux::persist::DataDirLease::acquire(paths.data_dir()).map_err(lease_error),
            |_| {
                shepr_api::start_server(api_tx, Arc::clone(&stop), &paths)
                    .map_err(RunServerError::from)
            },
            || reserve_client_socket_startup_lock(&expected).map_err(RunServerError::from),
        )
        .expect("reserve bootstrap resources");
        let server = reserved
            .restore_and_bind(
                |lease| {
                    app::App::with_paths(
                        &config,
                        &paths,
                        lease,
                        app::AppPolicy::Test,
                        super::super::sample_app_clock(),
                    )
                },
                |app, api, reservation| {
                    HeadlessServer::new(app, api_rx, Some(api), stop, reservation)
                },
            )
            .expect("bind through bootstrap reservation");
        assert_eq!(
            server.client_socket_path,
            client_socket_path(&server.app.paths)
        );
        assert_eq!(server.client_socket_path, expected);
        let _client = shepr_platform::ipc::connect_local_stream(&expected)
            .expect("the resolved client path has the listener");
        drop(server);
        assert!(!expected.try_exists().expect("client path released"));
    }

    fn resolve(raw: &std::ffi::OsStr) -> Option<PathBuf> {
        startup_cwd_from_env_value(shepr_core::env::resolve_path(
            shepr_core::env::EnvVar::SheprStartupCwd,
            Some(raw),
        ))
    }

    #[test]
    fn empty_startup_cwd_is_ignored() {
        assert_eq!(resolve(std::ffi::OsStr::new("")), None);
    }

    #[test]
    fn startup_cwd_value_becomes_path() {
        assert_eq!(
            resolve(std::ffi::OsStr::new("/srv/project")),
            Some(PathBuf::from("/srv/project"))
        );
    }

    #[test]
    fn startup_cwd_is_carried_byte_for_byte() {
        use std::os::unix::ffi::OsStrExt as _;
        for raw in [
            std::ffi::OsStr::from_bytes(b"/srv/caf\xe9"),
            std::ffi::OsStr::new("/srv/trailing space "),
        ] {
            assert_eq!(resolve(raw), Some(PathBuf::from(raw)));
        }
    }
}
