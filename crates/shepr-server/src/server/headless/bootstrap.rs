use super::*;

/// Why [`run_server`] refused to start or stopped with an error. The server
/// prints nothing itself: the binary renders this and picks the exit status.
#[derive(Debug)]
pub enum RunServerError {
    /// Another server already listens on `path`.
    AlreadyRunning { path: PathBuf },
    /// Another server already holds the lease on this profile's data
    /// directory, the canonical `directory`. The lease is taken before the
    /// socket is bound.
    DataDirHeld { directory: PathBuf },
    /// Startup or the event loop failed.
    Io(io::Error),
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
/// of [`run_server`] once the socket is bound and the TUI gate is open. Its
/// `Display` form is the operator notice a foreground server shows.
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

/// Runs the headless server, in this order: take the data-directory lease;
/// refuse a session path no save could replace; start file logging, the detection manifests and the integration installer;
/// bind the socket, which answers `ping` as `starting` from then on; build the
/// runtime; restore panes; build [`HeadlessServer`]; open the TUI gate;
/// report ready; run the loop. Shutdown keeps the socket through the final
/// save, retires the lease, then removes the socket
/// (`HeadlessServer::release_socket_after_save`).
///
/// `on_ready` runs once, after the TUI gate is open and before the event
/// loop starts; the binary uses it to tell a foreground operator where the
/// server listens. It runs on the tokio runtime, so it must not block.
pub fn run_server(
    config: &shepr_config::ValidatedServerConfig,
    paths: &shepr_config::AppPaths,
    on_ready: impl FnOnce(&ServerReady),
) -> Result<(), RunServerError> {
    let socket = paths.server_address().socket().to_path_buf();

    // AppPaths validated and retained the one-time startup handoff. Pane
    // launches scrub it from their child environments.
    let startup_cwd = paths.startup_cwd().map(std::path::Path::to_path_buf);

    let data_dir = paths.data_dir();

    let (api_tx, api_rx) = tokio::sync::mpsc::channel(crate::limits::API_REQUEST_CHANNEL_CAPACITY);
    let stop_signal = Arc::new(shepr_api::ServerStopSignal::default());

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
    shepr_mux::persist::check_session_target(&lease)?;
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
    // Everything a pane launch would otherwise do on its first spawn that may
    // block (the launch status listener, the passwd lookup, resolving this
    // binary's path), done before any pane is restored or created.
    shepr_mux::pane::init_pane_launches().map_err(startup_error)?;
    spawn_integration_install();
    let api =
        shepr_api::start_server(api_tx, Arc::clone(&stop_signal), paths).map_err(startup_error)?;
    let reserved = Reserved {
        lease,
        api,
        file_logging,
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async move {
        let Reserved {
            lease,
            api,
            file_logging,
        } = reserved;
        let mut app = app::App::with_paths(
            config,
            paths,
            lease,
            app::AppPolicy::Production,
            super::sample_app_clock(),
        );
        seed_startup_workspace_if_empty(&mut app, startup_cwd);
        let mut server = HeadlessServer::new(app, api_rx, api, stop_signal);
        server.open_client_protocol();
        let ready = ServerReady {
            socket,
            log_file: data_dir.join(shepr_platform::logging::SERVER_LOG_FILE),
            log_file_unavailable: file_logging.unavailable,
        };
        info!(socket = %ready.socket.display(), "shepr server started");
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
    if !integration_install_enabled(shepr_config::BuildProfile::current()) {
        info!("agent integration installation skipped; only release servers own agent configs");
        return;
    }
    let paths = shepr_agent::integration::AgentIntegrationPaths::resolve();
    if let Err(error) = std::thread::Builder::new()
        .name("integration-install".into())
        .spawn(move || {
            shepr_agent::integration::install_present_integrations(&paths);
        })
    {
        warn!(%error, "could not start the agent integration install");
    }
}

fn integration_install_enabled(profile: shepr_config::BuildProfile) -> bool {
    profile == shepr_config::BuildProfile::Release
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

/// Classifies a socket bind failure. A platform busy refusal means another
/// server owns the path; any other error, including an unrelated `AddrInUse`,
/// stays an IO failure. The refusal is recorded in the server log as well: a
/// daemonized server's stderr goes nowhere.
fn startup_error(error: io::Error) -> RunServerError {
    let Some(busy) = shepr_platform::ipc::SocketBusy::from_io(&error) else {
        return RunServerError::Io(error);
    };
    let path = busy.path().to_path_buf();
    tracing::error!(path = %path.display(), "another server already listens on the socket");
    RunServerError::AlreadyRunning { path }
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
mod startup_tests {
    use super::*;

    #[test]
    fn only_release_servers_install_shared_agent_integrations() {
        assert!(integration_install_enabled(
            shepr_config::BuildProfile::Release
        ));
        assert!(!integration_install_enabled(
            shepr_config::BuildProfile::Dev
        ));
    }

    #[tokio::test]
    async fn bootstrap_opens_the_gate_after_restore() {
        use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};
        use std::io::Write;
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("bootstrap-gate");
        let paths = shepr_config::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
            shepr_config::ServerConfig::default(),
            paths.clone(),
        );
        let lease = shepr_mux::persist::DataDirLease::acquire(paths.data_dir()).expect("lease");
        let (tx, rx) = mpsc::channel(crate::limits::API_REQUEST_CHANNEL_CAPACITY);
        let stop = Arc::new(shepr_api::ServerStopSignal::default());
        let api =
            shepr_api::start_server(tx, Arc::clone(&stop), &paths).expect("socket before restore");
        let client = shepr_api::client::ApiClient::for_socket(paths.server_address().socket());
        assert_eq!(
            client.status().expect("starting pong").lifecycle,
            shepr_api::RuntimeLifecycle::Starting
        );
        let app = app::App::with_paths(
            &config,
            &paths,
            lease,
            app::AppPolicy::Test,
            super::super::sample_app_clock(),
        );
        assert_eq!(
            client
                .status()
                .expect("restore alone leaves gate closed")
                .lifecycle,
            shepr_api::RuntimeLifecycle::Starting
        );
        let server = HeadlessServer::new(app, rx, api, stop);
        assert_eq!(
            client.status().expect("constructed pong").lifecycle,
            shepr_api::RuntimeLifecycle::Starting
        );
        server.open_client_protocol();
        assert_eq!(
            client.status().expect("ready pong").lifecycle,
            shepr_api::RuntimeLifecycle::Running
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
                    geometry: shepr_protocol::TerminalGeometry::new(80, 24, 8, 16, true),
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
        drop(server);
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
