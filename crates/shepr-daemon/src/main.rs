//! The `shepr-server` executable: the headless server, run in the foreground.
//!
//! It owns config validation for the serving host, the PTYs, persistence and
//! the socket listeners, and takes almost no arguments: `--version` prints the
//! build identity the client compares against, and the private
//! `--client-spawned` marks a launch by a shepr client.

use std::fmt::Display;
use std::process::ExitCode;

use shepr_launch::daemon_exit::DaemonExit;
use shepr_launch::guidance::server_ready_hint;
use shepr_launch::invocation::{
    SERVER_BINARY_NAME, ServerInvocation, server_usage, server_version_line,
};
use shepr_launch::limits::BOOT_LOG_PANIC_REPORTS;
use shepr_launch::process_status::ProcessStatus;
use shepr_server::{RunServerError, ServerReady, run_server};

fn main() -> ExitCode {
    let args: Vec<String> = match std::env::args_os()
        .skip(1)
        .map(std::ffi::OsString::into_string)
        .collect()
    {
        Ok(args) => args,
        Err(_) => return usage_error("arguments must be valid UTF-8"),
    };
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    match ServerInvocation::parse(&args) {
        Some(ServerInvocation::Serve { client_spawned }) => serve(client_spawned),
        Some(ServerInvocation::Version) => {
            shepr_platform::begin_cli_output();
            println!("{}", server_version_line());
            ExitCode::SUCCESS
        }
        None => usage_error(&format!("unexpected arguments: {}", args.join(" "))),
    }
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    eprintln!("{}", server_usage());
    ExitCode::from(ProcessStatus::Usage.code())
}

/// Validates the config for the serving host and runs the server until it
/// stops. The working directory a client handed over travels in
/// `SHEPR_STARTUP_CWD`, which `resolve_for_server` reads.
///
/// A client-spawned server has the client's boot log as its stderr. Once the
/// server log is running (the ready callback), stderr goes to `/dev/null` so
/// the boot log holds only pre-logging failures; a panic from then on reaches
/// the server log through the panic hook `run_server` installs. A
/// client-spawned server whose log file could not be opened has no other
/// record, so it keeps the boot log as stderr and bounds it instead: after
/// readiness the only writes to stderr are panic reports and the exit error,
/// and only the first [`BOOT_LOG_PANIC_REPORTS`] panics are reported. A
/// foreground server keeps its stderr.
fn serve(client_spawned: bool) -> ExitCode {
    let paths = match shepr_paths::AppPaths::resolve_for_server() {
        Ok(paths) => paths,
        Err(errors) => return config_error(errors.messages()),
    };
    let config = match shepr_config::load_server_validated(&paths) {
        Ok(config) => config,
        Err(diagnostics) => return config_error(&diagnostics),
    };
    let on_ready = |ready: &ServerReady| {
        if !client_spawned {
            eprintln!("{ready}\n{}", server_ready_hint());
        } else if ready.log_file_unavailable.is_some() {
            // Without a server log the boot log is the only place a later
            // panic is recorded, so it stays the stderr, bounded by the cap.
            eprintln!("{ready}\n{}", server_ready_hint());
            cap_panic_reports();
        } else if let Err(error) = shepr_platform::redirect_stderr_to_null() {
            // Stderr is still the boot log; one line there is bounded.
            eprintln!("{SERVER_BINARY_NAME}: could not detach stderr from the boot log: {error}");
        }
    };
    match run_server(&config, config.paths(), on_ready) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report_server_error(error),
    }
}

/// Wraps the panic hook so only the first [`BOOT_LOG_PANIC_REPORTS`] panics
/// reach it; later ones are not reported.
fn cap_panic_reports() {
    let previous = std::panic::take_hook();
    let reported = std::sync::atomic::AtomicUsize::new(0);
    std::panic::set_hook(Box::new(move |info| {
        if reported.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < BOOT_LOG_PANIC_REPORTS {
            previous(info);
        }
    }));
}

fn exit_with(class: DaemonExit) -> ExitCode {
    ExitCode::from(class.process_status().code())
}

fn config_error<I, D>(diagnostics: I) -> ExitCode
where
    I: IntoIterator<Item = D>,
    D: Display,
{
    eprintln!("{SERVER_BINARY_NAME}: configuration error:");
    for diagnostic in diagnostics {
        eprintln!("  {diagnostic}");
    }
    exit_with(DaemonExit::ConfigRefused)
}

/// Maps a `RunServerError` onto `shepr_launch::daemon_exit::DaemonExit`. The
/// classification stays here in the daemon main, not in shepr-launch, because
/// it matches `RunServerError`, which lives in shepr-server, a crate above
/// shepr-launch; launch owns only the `DaemonExit` vocabulary it maps onto.
///
/// A server already holding the runtime, by either socket or by the data lock,
/// reads the same to the operator and ends with the same exit code.
fn report_server_error(error: RunServerError) -> ExitCode {
    let already_running = shepr_launch::guidance::server_already_running();
    match error {
        RunServerError::AlreadyRunning { path } => {
            eprintln!("error: {already_running}");
            eprintln!("socket: {}", path.display());
            exit_with(DaemonExit::AlreadyRunning)
        }
        RunServerError::DataDirHeld { directory } => {
            eprintln!("error: {already_running}");
            eprintln!("data directory: {}", directory.display());
            exit_with(DaemonExit::AlreadyRunning)
        }
        RunServerError::SessionTarget(error)
        | RunServerError::PaneLaunch(error)
        | RunServerError::Socket(error)
        | RunServerError::Runtime(error)
        | RunServerError::Lease(error)
        | RunServerError::Logging(error)
        | RunServerError::SignalInstall(error) => {
            eprintln!("error: {error}");
            exit_with(DaemonExit::Failed)
        }
        RunServerError::Shutdown(error) => {
            eprintln!("error: {error}");
            exit_with(DaemonExit::Failed)
        }
    }
}
