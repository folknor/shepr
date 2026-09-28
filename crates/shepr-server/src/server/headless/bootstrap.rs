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
    /// Another server for this session already listens on `path`.
    AlreadyRunning { socket: ServerSocket, path: PathBuf },
    /// Startup or the event loop failed.
    Io(io::Error),
}

impl std::fmt::Display for RunServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { socket, path } => write!(
                f,
                "shepr server is already running ({socket}: {})",
                path.display()
            ),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RunServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AlreadyRunning { .. } => None,
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for RunServerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<RunServerError> for io::Error {
    fn from(error: RunServerError) -> Self {
        match error {
            RunServerError::Io(error) => error,
            already_running @ RunServerError::AlreadyRunning { .. } => {
                io::Error::new(io::ErrorKind::AddrInUse, already_running.to_string())
            }
        }
    }
}

/// Where a started server listens and logs, handed to the `on_ready` callback
/// of [`run_server`] once both sockets are bound. Its `Display` form is the
/// operator notice a foreground `shepr server` shows.
#[derive(Clone, Debug)]
pub struct ServerReady {
    pub api_socket: PathBuf,
    pub client_socket: PathBuf,
    pub log_file: PathBuf,
}

impl std::fmt::Display for ServerReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "shepr server running; you can use any shepr CLI command in another terminal."
        )?;
        writeln!(f, "api socket: {}", self.api_socket.display())?;
        writeln!(f, "client socket: {}", self.client_socket.display())?;
        writeln!(f, "logs: {}", self.log_file.display())?;
        write!(
            f,
            "did you mean to open the Shepr TUI? run `shepr`; you do not need `shepr server`."
        )
    }
}

/// Run the headless server. This is the entry point called from main.rs.
///
/// `on_ready` runs once, after both sockets are bound and before the event
/// loop starts; the binary uses it to tell a foreground operator where the
/// server listens. It runs on the tokio runtime, so it must not block.
pub fn run_server(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
    on_ready: impl FnOnce(&ServerReady),
) -> Result<(), RunServerError> {
    let resolved_config = encode_resolved_config(config)?;

    // Consume the startup-cwd hint before anything below starts a thread: the
    // API server thread, the tokio workers and session restore all run
    // concurrently afterwards, and unsetting a variable while another thread
    // may call getenv is undefined behaviour in glibc. `main` reaches this
    // function without having spawned any thread; keep it that way.
    let startup_cwd = take_startup_cwd();

    let session_data_dir = shepr_api::session::data_dir(paths);
    let lease = shepr_mux::persist::DataDirLease::acquire(&session_data_dir)?;

    shepr_platform::logging::init_file_logging(
        &shepr_api::session::data_dir(paths),
        shepr_platform::logging::SERVER_LOG_FILE,
    )?;
    // Compile the full registry off the tokio loop, and before App restores PTYs
    // whose detection workers can consult it. After logging starts, so manifest
    // override diagnostics reach the server log.
    let agent_manifest_summaries =
        shepr_agent::detect::manifest::reload_manifests(paths.config_dir());

    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = shepr_api::EventHub::default();
    let stop_requested = Arc::new(AtomicBool::new(false));

    // Start the JSON API socket server.
    let _api_server = match shepr_api::start_server_with_stop_control(
        api_tx.clone(),
        event_hub.clone(),
        Arc::clone(&stop_requested),
        paths,
    ) {
        Ok(server) => server,
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            return Err(already_running(
                ServerSocket::Api,
                shepr_api::socket_path(paths),
            ));
        }
        Err(err) => return Err(err.into()),
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
            app::AppPolicy::PRODUCTION,
            api_rx,
            event_hub,
            agent_manifest_summaries,
        );
        seed_startup_workspace_if_empty(&mut app, startup_cwd);

        // Create the headless server.
        let mut server =
            match HeadlessServer::new(app, Some(_api_server), resolved_config, stop_requested) {
                Ok(server) => server,
                Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                    return Err(already_running(
                        ServerSocket::Client,
                        client_socket_path(paths),
                    ));
                }
                Err(err) => return Err(err.into()),
            };

        let ready = ServerReady {
            api_socket: shepr_api::socket_path(paths),
            client_socket: client_socket_path(paths),
            log_file: session_data_dir.join(shepr_platform::logging::SERVER_LOG_FILE),
        };
        info!(
            api_socket = %ready.api_socket.display(),
            client_socket = %ready.client_socket.display(),
            "shepr server started"
        );
        on_ready(&ready);

        server.run().await.map_err(RunServerError::from)
    });

    rt.shutdown_timeout(Duration::from_millis(100));
    shepr_platform::logging::shutdown("server");
    result
}

fn encode_resolved_config(config: &shepr_config::ValidatedConfig) -> io::Result<Vec<u8>> {
    let mut encoded = Vec::new();
    shepr_protocol::codec::encode_into(&mut encoded, config).map_err(|error| {
        io::Error::other(format!(
            "validated configuration could not be encoded for the client protocol: {error}"
        ))
    })?;
    Ok(encoded)
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

    match app.create_workspace_with_options(&cwd, true) {
        Ok(_) => {
            info!(cwd = %cwd.display(), "created startup workspace");
        }
        Err(err) => {
            warn!(cwd = %cwd.display(), err = %err, "failed to create startup workspace");
            app.state.mode = app::Mode::Navigate;
        }
    }
}

/// Read and unset the startup-cwd hint the spawning client left in the
/// environment, so pane shells do not inherit it.
///
/// Must run while the process is still single-threaded; see `run_server`.
fn take_startup_cwd() -> Option<PathBuf> {
    let var = shepr_core::env::EnvVar::SheprStartupCwd;
    let cwd = startup_cwd_from_env_value(shepr_core::env::read_path(var));
    // SAFETY: `run_server` calls this before it starts the API server thread,
    // the tokio runtime or anything else that spawns threads, and `main` spawns
    // none before calling `run_server`, so no other thread can be reading the
    // environment concurrently.
    unsafe { shepr_core::env::remove(var) };
    cwd
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

/// The refusal for a socket another server holds, recorded in the server log
/// as well: a daemonized server's stderr goes nowhere.
fn already_running(socket: ServerSocket, path: PathBuf) -> RunServerError {
    tracing::error!(%socket, path = %path.display(), "shepr server is already running");
    RunServerError::AlreadyRunning { socket, path }
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
