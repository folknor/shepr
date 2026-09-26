use super::*;

/// Run the headless server. This is the entry point called from main.rs.
pub fn run_server(config: &config::Config, paths: &config::AppPaths) -> io::Result<()> {
    // Consume the startup-cwd hint before anything below starts a thread: the
    // API server thread, the tokio workers and session restore all run
    // concurrently afterwards, and unsetting a variable while another thread
    // may call getenv is undefined behaviour in glibc. `main` reaches this
    // function without having spawned any thread; keep it that way.
    let startup_cwd = take_startup_cwd();

    let session_data_dir = crate::session::data_dir(paths);
    if let Err(err) = crate::persist::lock::claim(&session_data_dir) {
        eprintln!("error: cannot claim shepr session directory: {err}");
        std::process::exit(1);
    }

    crate::logging::init_file_logging(paths, crate::logging::SERVER_LOG_FILE);

    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();
    let should_quit = Arc::new(AtomicBool::new(false));

    // Start the JSON API socket server.
    let _api_server = match api::start_server_with_stop_control(
        api_tx.clone(),
        event_hub.clone(),
        Arc::clone(&should_quit),
        paths,
    ) {
        Ok(server) => server,
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            eprintln!("error: shepr server is already running");
            eprintln!("api socket: {}", api::socket_path(paths).display());
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
        let mut app =
            app::App::with_paths(config, paths, app::AppPolicy::PRODUCTION, api_rx, event_hub);
        seed_startup_workspace_if_empty(&mut app, startup_cwd);

        // Create the headless server.
        let mut server = match HeadlessServer::new(app, Some(_api_server), should_quit) {
            Ok(server) => server,
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                eprintln!("error: shepr server is already running");
                eprintln!("client socket: {}", client_socket_path(paths).display());
                std::process::exit(1);
            }
            Err(err) => return Err(err),
        };

        info!(
            api_socket = %api::socket_path(paths).display(),
            client_socket = %client_socket_path(paths).display(),
            "shepr server started"
        );
        print_ready_message(
            &api::socket_path(paths),
            &client_socket_path(paths),
            &session_data_dir,
        );

        server.run().await
    });

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("server");
    result
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
    let cwd = std::env::var_os(crate::server::autodetect::STARTUP_CWD_ENV_VAR)?;
    // SAFETY: `run_server` calls this before it starts the API server thread,
    // the tokio runtime or anything else that spawns threads, and `main` spawns
    // none before calling `run_server`, so no other thread can be reading the
    // environment concurrently.
    unsafe { std::env::remove_var(crate::server::autodetect::STARTUP_CWD_ENV_VAR) };
    startup_cwd_from_env_value(cwd)
}

fn startup_cwd_from_env_value(value: std::ffi::OsString) -> Option<PathBuf> {
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn print_ready_message(api_socket: &Path, client_socket: &Path, session_data_dir: &Path) {
    eprintln!("shepr server running; you can use any shepr CLI command in another terminal.");
    eprintln!("api socket: {}", api_socket.display());
    eprintln!("client socket: {}", client_socket.display());
    eprintln!(
        "logs: {}",
        session_data_dir
            .join(crate::logging::SERVER_LOG_FILE)
            .display()
    );
    eprintln!("did you mean to open the Shepr TUI? run `shepr`; you do not need `shepr server`.");
}

#[cfg(test)]
mod startup_cwd_tests {
    use super::*;

    #[test]
    fn empty_startup_cwd_is_ignored() {
        assert_eq!(startup_cwd_from_env_value(std::ffi::OsString::new()), None);
    }

    #[test]
    fn startup_cwd_value_becomes_path() {
        assert_eq!(
            startup_cwd_from_env_value(std::ffi::OsString::from("/srv/project")),
            Some(PathBuf::from("/srv/project"))
        );
    }
}
