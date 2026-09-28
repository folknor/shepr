use super::*;

/// Run the headless server. This is the entry point called from main.rs.
pub fn run_server(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
) -> io::Result<()> {
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
            eprintln!("error: shepr server is already running");
            eprintln!("api socket: {}", shepr_api::socket_path(paths).display());
            std::process::exit(1);
        }
        Err(err) => return Err(err),
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
                    eprintln!("error: shepr server is already running");
                    eprintln!("client socket: {}", client_socket_path(paths).display());
                    std::process::exit(1);
                }
                Err(err) => return Err(err),
            };

        info!(
            api_socket = %shepr_api::socket_path(paths).display(),
            client_socket = %client_socket_path(paths).display(),
            "shepr server started"
        );
        print_ready_message(
            &shepr_api::socket_path(paths),
            &client_socket_path(paths),
            &session_data_dir,
        );

        server.run().await
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

fn print_ready_message(api_socket: &Path, client_socket: &Path, session_data_dir: &Path) {
    eprintln!("shepr server running; you can use any shepr CLI command in another terminal.");
    eprintln!("api socket: {}", api_socket.display());
    eprintln!("client socket: {}", client_socket.display());
    eprintln!(
        "logs: {}",
        session_data_dir
            .join(shepr_platform::logging::SERVER_LOG_FILE)
            .display()
    );
    eprintln!("did you mean to open the Shepr TUI? run `shepr`; you do not need `shepr server`.");
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
