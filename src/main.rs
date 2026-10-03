use std::io;

use cli::{CliError, CliResult};

/// Exit contracts decoded at the CLI process boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessExit {
    Success,
    Failed,
    Usage,
    Stop(shepr_api::server_stop::ServerStopExit),
}

impl ProcessExit {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failed => 1,
            Self::Usage => 2,
            // ServerStopExit encodes a closed set of process exit bytes, so the
            // fallback to a plain failure is unreachable.
            Self::Stop(exit) => u8::try_from(exit.code()).unwrap_or(1),
        }
    }

    pub(crate) fn from_cli_code(code: i32) -> Self {
        match code {
            0 => Self::Success,
            1 => Self::Failed,
            2 => Self::Usage,
            code => match shepr_api::server_stop::ServerStopExit::from_code(code) {
                Some(exit) => Self::Stop(exit),
                None => {
                    tracing::error!(code, "CLI returned an invalid process exit status");
                    Self::Failed
                }
            },
        }
    }
}

impl std::process::Termination for ProcessExit {
    fn report(self) -> std::process::ExitCode {
        std::process::ExitCode::from(self.code())
    }
}

const NESTED_SHEPR_MESSAGES: &[&str] = &[
    "inception detected. we need to go deeper... said no one ever.",
    "recursion is a pathway to many abilities some consider to be... unnatural.",
    "you were so preoccupied with whether you could, you didn't stop to think if you should. - dr. malcolm",
    "recursive shepring is disabled. somewhere, a call stack breathes a sigh of relief.",
    "recursive descent denied. there is, in fact, such a thing as too much shepr.",
    "recursion detected. base case not found. aborting.",
];

mod autodetect;
mod cli;
mod limits;
mod preflight;

fn random_nested_message() -> &'static str {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.subsec_nanos() as usize);
    let index = (nanos ^ (std::process::id() as usize)) % NESTED_SHEPR_MESSAGES.len();
    NESTED_SHEPR_MESSAGES[index]
}

fn args_as_utf8<I>(args: I) -> Result<Vec<String>, String>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    args.into_iter()
        .enumerate()
        .map(|(index, arg)| {
            arg.into_string()
                .map_err(|_| format!("argument {index} is not valid UTF-8"))
        })
        .collect()
}

/// The one place the process ends. Every launch below returns its exit status
/// or a typed [`CliError`]; the error is printed here, once, and its
/// `exit_status` becomes the status. Returning from `main` rather than calling
/// `std::process::exit` lets the destructors of everything `launch` held run
/// first (the client's terminal guard, SSH teardown registrations).
fn main() -> ProcessExit {
    match launch() {
        Ok(code) => code,
        Err(error) => {
            error.print();
            error.exit_status()
        }
    }
}

fn launch() -> CliResult<ProcessExit> {
    let raw_args: Vec<String> = args_as_utf8(std::env::args_os()).map_err(CliError::Usage)?;
    launch_with_args(&raw_args)
}

fn launch_with_args(raw_args: &[String]) -> CliResult<ProcessExit> {
    // The one command-line parser: the clap spec in `cli/spec.rs`. It prints
    // its own usage errors and subcommand help, and hands back only the status.
    let launch = match cli::parse_launch(raw_args) {
        Ok(launch) => launch,
        Err(exit_code) => return Ok(ProcessExit::from_cli_code(exit_code)),
    };

    match launch {
        cli::Launch::Help => {
            cli::print_help();
            Ok(ProcessExit::Success)
        }
        cli::Launch::Version => {
            shepr_platform::begin_cli_output();
            println!("shepr {}", shepr_protocol::build_version());
            Ok(ProcessExit::Success)
        }
        cli::Launch::ClientBridge => {
            let paths = resolve_bridge_paths()?;
            init_client_logging(&paths)?;
            finish_bridge(shepr_remote::run_remote_client_bridge(&paths)?)
        }
        cli::Launch::Cli(command) => cli::run(&command).map(ProcessExit::from_cli_code),
        cli::Launch::Client => launch_client(ClientLaunch::Direct),
        cli::Launch::Tui => launch_client(ClientLaunch::Tui),
    }
}

#[derive(Clone, Copy)]
enum ClientLaunch {
    Direct,
    Tui,
}

fn launch_client(mode: ClientLaunch) -> CliResult<ProcessExit> {
    // Resolve the typed pane markers before reading client.toml. A same-profile
    // pane is refused even when the file is broken.
    let paths = shepr_paths::AppPaths::resolve_for_client()
        .map_err(CliError::from)?
        .ok_or_else(|| CliError::Nested {
            quip: random_nested_message(),
        })?;
    let loaded_config = shepr_config::load_client_validated(&paths).map_err(CliError::Config)?;
    let paths = loaded_config.paths();

    match mode {
        ClientLaunch::Direct => {
            init_client_logging(paths)?;
            cli::finish_client(shepr_client::run_client(&loaded_config, paths))
                .map(ProcessExit::from_cli_code)
        }
        ClientLaunch::Tui => launch_tui(&loaded_config, paths),
    }
}

fn launch_tui(
    loaded_config: &shepr_config::ValidatedClientConfig,
    paths: &shepr_paths::AppPaths,
) -> CliResult<ProcessExit> {
    autodetect::ensure_terminal_geometry()
        .map_err(|error| CliError::Client(shepr_client::ClientRunError::Launch(error)))?;

    init_client_logging(paths)?;
    // Prompts and restart offers must run before the client takes the
    // terminal: it connects to machines with BatchMode and cannot answer one.
    let connectors = preflight::run(loaded_config, paths);
    let client = autodetect::auto_detect_launch(
        loaded_config,
        paths,
        shepr_remote::local_server::SERVER_READY_TIMEOUT,
        connectors,
        shepr_client::run_client_with_connectors,
    )
    .map_err(CliError::Launch)?;
    cli::finish_client(client).map(ProcessExit::from_cli_code)
}

/// A bridge that ended on its idle watchdog logs the measured idle duration
/// and ends the process with status 1. Its relay threads may still hold stdin
/// and stdout, so the caller must not join them or write to stdout.
fn finish_bridge(outcome: shepr_platform::RemoteBridgeOutcome) -> CliResult<ProcessExit> {
    match outcome {
        shepr_platform::RemoteBridgeOutcome::Closed => Ok(ProcessExit::Success),
        shepr_platform::RemoteBridgeOutcome::IdleExpired { idle_for } => {
            tracing::warn!(idle_for = ?idle_for, "remote bridge idle timeout expired");
            Err(CliError::BridgeIdle)
        }
    }
}

/// Installs the process-wide client logger before client or bridge code logs.
/// Every path into `shepr_client` (the TUI launch and the `client` command)
/// and the remote client bridge call it once; the client library installs none
/// of its own. The bridge writes to the host's client log, never stdout, which
/// carries the relayed stream. A log file that cannot be opened is reported on
/// stderr.
fn init_client_logging(paths: &shepr_paths::AppPaths) -> io::Result<()> {
    let logging_config = shepr_platform::logging::FileLoggingConfig::from_environment()?;
    let outcome =
        shepr_platform::logging::init_client_file_logging(paths.data_dir(), logging_config)?;
    if let Some(unavailable) = outcome.unavailable {
        cli::print_notice(&format!(
            "shepr: could not initialize file logging at {}: {}",
            unavailable.path.display(),
            unavailable.reason
        ));
    }
    Ok(())
}

fn resolve_bridge_paths() -> CliResult<shepr_paths::AppPaths> {
    shepr_paths::AppPaths::resolve().map_err(CliError::from)
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_core::env::{EnvVar, SHEPR_ENV_IN_PANE};

    #[test]
    fn nested_shepr_blocks_when_env_is_set() {
        let env = crate::test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprEnv, SHEPR_ENV_IN_PANE);
        assert!(
            shepr_paths::AppPaths::resolve_for_client()
                .expect("resolve marker")
                .is_none()
        );
    }

    #[test]
    fn nested_shepr_blocks_with_a_matching_profile() {
        let env = crate::test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprEnv, SHEPR_ENV_IN_PANE);
        env.set(
            EnvVar::SheprBuildProfile,
            shepr_paths::BuildProfile::current().marker(),
        );
        assert!(
            shepr_paths::AppPaths::resolve_for_client()
                .expect("resolve marker")
                .is_none()
        );
    }

    #[test]
    fn nested_shepr_does_not_block_without_env() {
        let _env = crate::test_support::IsolatedEnv::new();
        assert!(
            shepr_paths::AppPaths::resolve_for_client()
                .expect("resolve paths")
                .is_some()
        );
    }

    #[test]
    fn server_stop_does_not_load_a_broken_config() {
        let env = crate::test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("broken-launch-config");
        env.set(EnvVar::XdgConfigHome, scratch.path());
        let paths = shepr_paths::AppPaths::resolve().expect("resolve paths");
        std::fs::create_dir_all(paths.config_dir()).expect("create config directory");
        let config = paths.client_config_file();
        std::fs::write(&config, "this = [not valid TOML").expect("write broken config");
        std::fs::write(paths.server_config_file(), "this = [not valid TOML")
            .expect("write broken server config");
        let args = ["shepr", "server", "stop"].map(str::to_owned);

        let result = launch_with_args(&args);
        assert!(
            matches!(&result, Err(CliError::ServerStop(_))),
            "server stop should reach its local server check without parsing config: {result:?}"
        );
    }

    #[test]
    fn a_nested_launch_is_refused_before_a_broken_client_config_is_read() {
        let env = crate::test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("nested-broken-config");
        env.set(EnvVar::XdgConfigHome, scratch.path());
        env.set(EnvVar::SheprEnv, SHEPR_ENV_IN_PANE);
        env.set(
            EnvVar::SheprBuildProfile,
            shepr_paths::BuildProfile::current().marker(),
        );
        let paths = shepr_paths::AppPaths::resolve().expect("resolve paths");
        std::fs::create_dir_all(paths.config_dir()).expect("create config directory");
        std::fs::write(paths.client_config_file(), "this = [not valid TOML")
            .expect("write broken client config");

        let result = launch_with_args(&["shepr".to_owned()]);
        assert!(
            matches!(&result, Err(CliError::Nested { .. })),
            "a same-profile pane is refused before config loads: {result:?}"
        );
    }

    #[test]
    fn a_different_profile_pane_does_not_block_nesting() {
        let env = crate::test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprEnv, SHEPR_ENV_IN_PANE);
        let other_profile = match shepr_paths::BuildProfile::current() {
            shepr_paths::BuildProfile::Release => "dev",
            shepr_paths::BuildProfile::Dev => "release",
        };
        env.set(EnvVar::SheprBuildProfile, other_profile);
        assert!(
            shepr_paths::AppPaths::resolve_for_client()
                .expect("resolve paths")
                .is_some()
        );
    }

    #[test]
    fn nested_message_strings_no_longer_repeat_shepr_prefix() {
        assert!(
            NESTED_SHEPR_MESSAGES
                .iter()
                .all(|message| !message.starts_with("shepr:"))
        );
    }

    fn invalid_utf8_arg() -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![0xff])
    }

    #[test]
    fn args_as_utf8_passes_through_valid_arguments() {
        let args = ["shepr", "pane", "get", "pane-1"].map(std::ffi::OsString::from);
        assert_eq!(
            args_as_utf8(args).expect("test precondition"),
            ["shepr", "pane", "get", "pane-1"]
        );
    }

    #[test]
    fn args_as_utf8_reports_the_offending_argument_instead_of_panicking() {
        let args = vec![
            std::ffi::OsString::from("shepr"),
            std::ffi::OsString::from("pane"),
            invalid_utf8_arg(),
        ];
        assert_eq!(
            args_as_utf8(args).expect_err("test precondition"),
            "argument 2 is not valid UTF-8"
        );
    }
}
