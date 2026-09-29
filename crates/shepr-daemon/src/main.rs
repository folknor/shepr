//! The `shepr-server` executable: the headless server, run in the foreground.
//!
//! It owns config validation for the serving host, the PTYs, persistence and
//! the socket listeners, and takes almost no arguments: `--version` prints the
//! build identity the client compares against, and the private
//! `--client-spawned` marks a launch by a shepr client.

use std::process::ExitCode;

use shepr_api::daemon_exit::{
    ALREADY_RUNNING_EXIT_CODE, CLIENT_SPAWNED_FLAG, CONFIG_REFUSED_EXIT_CODE, FAILED_EXIT_CODE,
};
use shepr_server::server::headless::{RunServerError, run_server};

const VERSION_FLAG: &str = "--version";

fn main() -> ExitCode {
    let args: Vec<String> = match std::env::args_os()
        .skip(1)
        .map(std::ffi::OsString::into_string)
        .collect()
    {
        Ok(args) => args,
        Err(_) => return usage_error("arguments must be valid UTF-8"),
    };
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | [CLIENT_SPAWNED_FLAG] => serve(),
        [VERSION_FLAG] => {
            shepr_platform::begin_cli_output();
            println!("shepr-server {}", shepr_protocol::build_version());
            ExitCode::SUCCESS
        }
        other => usage_error(&format!("unexpected arguments: {}", other.join(" "))),
    }
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    eprintln!("usage: shepr-server [--version]");
    ExitCode::from(2)
}

/// Validates the config for the serving host and runs the server until it
/// stops. The working directory a client handed over travels in
/// `SHEPR_STARTUP_CWD`, which `resolve_for_server` reads.
fn serve() -> ExitCode {
    let paths = match shepr_config::AppPaths::resolve_for_server() {
        Ok(paths) => paths,
        Err(errors) => return config_error(&errors),
    };
    let config = match shepr_config::load_validated(&paths) {
        Ok(config) => config,
        Err(diagnostics) => {
            let errors: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
            return config_error(&errors);
        }
    };
    match run_server(&config, config.paths(), |ready| eprintln!("{ready}")) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report_server_error(error),
    }
}

fn exit_with(code: i32) -> ExitCode {
    // The codes are small positive constants, so the conversion cannot fail;
    // a failure would still end the process as a plain failure.
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

fn config_error(diagnostics: &[String]) -> ExitCode {
    eprintln!("shepr-server: configuration error:");
    for diagnostic in diagnostics {
        eprintln!("  {diagnostic}");
    }
    exit_with(CONFIG_REFUSED_EXIT_CODE)
}

/// A server already holding the runtime, by either socket or by the data lock,
/// reads the same to the operator and ends with the same exit code.
fn report_server_error(error: RunServerError) -> ExitCode {
    const ALREADY_RUNNING: &str = "shepr-server is already running";
    match error {
        RunServerError::AlreadyRunning { socket, path } => {
            eprintln!("error: {ALREADY_RUNNING}");
            eprintln!("{socket}: {}", path.display());
            exit_with(ALREADY_RUNNING_EXIT_CODE)
        }
        RunServerError::SessionDataHeld { directory } => {
            eprintln!("error: {ALREADY_RUNNING}");
            eprintln!("data directory: {}", directory.display());
            exit_with(ALREADY_RUNNING_EXIT_CODE)
        }
        RunServerError::Io(error) => {
            eprintln!("error: {error}");
            exit_with(FAILED_EXIT_CODE)
        }
    }
}
