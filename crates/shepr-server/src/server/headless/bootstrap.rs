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
    let lease = shepr_mux::persist::DataDirLease::acquire(data_dir).map_err(lease_error)?;

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

    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let stop_requested = Arc::new(shepr_api::ServerStopSignal::default());

    // Start the JSON API socket server.
    let _api_server =
        match shepr_api::start_server(api_tx.clone(), Arc::clone(&stop_requested), paths) {
            Ok(server) => server,
            Err(err) => return Err(startup_error(ServerSocket::Api, err)),
        };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async {
        // Create the App (with AppState, event channels, etc.).
        let mut app = app::App::with_paths(
            config,
            paths,
            lease,
            app::AppPolicy::Production,
            api_rx,
            super::sample_app_clock(),
        );
        seed_startup_workspace_if_empty(&mut app, startup_cwd);

        // Create the headless server.
        let mut server = match HeadlessServer::new(app, Some(_api_server), stop_requested) {
            Ok(server) => server,
            Err(err) => return Err(startup_error(ServerSocket::Client, err)),
        };

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
/// the file IO and any agent version probe stay off the startup path. The
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

/// Classifies a failure binding `socket`. Only the busy refusal from
/// `shepr_platform::ipc`, which names the path another server holds, means a
/// server is already running; any other error, including an unrelated
/// `AddrInUse`, stays an IO failure. The refusal is recorded in the server log
/// as well: a daemonized server's stderr goes nowhere.
fn startup_error(socket: ServerSocket, error: io::Error) -> RunServerError {
    let Some(busy) = shepr_platform::ipc::SocketBusy::from_io(&error) else {
        return RunServerError::Io(error);
    };
    let path = busy.path().to_path_buf();
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
mod startup_cwd_tests {
    use super::*;

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
