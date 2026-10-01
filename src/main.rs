use std::io;
use std::process::ExitCode;

use cli::{CliError, CliResult};
use shepr_core::env::{EnvVar, SHEPR_ENV_IN_PANE};

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

/// Refuses a TUI or client launch from a pane of a server of this build
/// profile. `SHEPR_ENV` counts only when it is exactly [`SHEPR_ENV_IN_PANE`],
/// the value shepr writes and the hook assets check. A pane whose profile
/// marker names the other profile has separate sockets and may launch this
/// TUI; a pane with no marker is refused like a matching one.
fn refuse_if_nested() -> CliResult<()> {
    let shepr_env = shepr_core::env::read_text(EnvVar::SheprEnv)
        .map_err(|error| CliError::Config(vec![error.to_string()]))?;
    let owner_profile = shepr_core::env::read_text(EnvVar::SheprBuildProfile)
        .map_err(|error| CliError::Config(vec![error.to_string()]))?;
    let blocked = should_block_nested_for_env(shepr_env.as_deref(), owner_profile.as_deref());
    if blocked {
        return Err(CliError::Nested {
            quip: random_nested_message(),
        });
    }
    Ok(())
}

fn should_block_nested_for_env(shepr_env: Option<&str>, owner_profile: Option<&str>) -> bool {
    let profile_matches_or_is_unknown = owner_profile
        .is_none_or(|profile| profile == shepr_config::BuildProfile::current().marker());
    shepr_env == Some(SHEPR_ENV_IN_PANE) && profile_matches_or_is_unknown
}

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
/// `exit_code` becomes the status. Returning from `main` rather than calling
/// `std::process::exit` lets the destructors of everything `launch` held run
/// first (the client's terminal guard, SSH teardown registrations).
fn main() -> ExitCode {
    let code = match launch() {
        Ok(code) => code,
        Err(error) => {
            error.print();
            error.exit_code()
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn launch() -> CliResult<i32> {
    let raw_args: Vec<String> = args_as_utf8(std::env::args_os()).map_err(CliError::Usage)?;
    launch_with_args(&raw_args)
}

fn launch_with_args(raw_args: &[String]) -> CliResult<i32> {
    // The one command-line parser: the clap spec in `cli/spec.rs`. It prints
    // its own usage errors and subcommand help, and hands back only the status.
    let invocation = match cli::parse_invocation(raw_args) {
        Ok(invocation) => invocation,
        Err(exit_code) => return Ok(exit_code),
    };

    // Root-level `--help` and `--version` win over any
    // subcommand given with them.
    if invocation.help_requested() {
        cli::print_help();
        return Ok(0);
    }

    if invocation.version_requested() {
        shepr_platform::begin_cli_output();
        println!("shepr {}", shepr_protocol::build_version());
        return Ok(0);
    }

    if let Some(command) = invocation.cli_command() {
        return cli::run(command);
    }

    if matches!(invocation.launch, cli::Launch::ClientBridge) {
        let paths = resolve_bridge_paths()?;
        init_client_logging(&paths)?;
        return finish_bridge(shepr_remote::run_remote_client_bridge(&paths)?);
    }

    // Every launch left is the TUI or its internal client launch. Neither runs
    // inside a pane of a server of this profile, so refuse before client.toml
    // is read: a broken file must not hide the refusal.
    refuse_if_nested()?;

    let loaded_config = load_validated_config(shepr_config::AppPaths::resolve())?;
    let paths = loaded_config.paths();

    match invocation.launch {
        cli::Launch::Client => {
            init_client_logging(paths)?;
            return cli::finish_client(shepr_client::run_client(&loaded_config, paths));
        }
        cli::Launch::Tui => {}
        cli::Launch::ClientBridge | cli::Launch::Cli(_) => {
            return Err(io::Error::other("launch was already handled").into());
        }
    }

    autodetect::ensure_terminal_geometry()
        .map_err(|error| CliError::Client(shepr_client::ClientRunError::Launch(error)))?;

    init_client_logging(paths)?;
    // Prompts and restart offers must run before the client takes the
    // terminal: it connects to machines with BatchMode and cannot answer one.
    preflight::run(&loaded_config, paths);
    let client = autodetect::auto_detect_launch(
        &loaded_config,
        paths,
        limits::SERVER_READY_TIMEOUT,
        shepr_client::run_client,
    )
    .map_err(|error| CliError::Client(shepr_client::ClientRunError::Launch(error)))?;
    cli::finish_client(client)
}

/// A bridge that ended on its idle watchdog logs the measured idle duration
/// and ends the process with status 1. Its relay threads may still hold stdin
/// and stdout, so the caller must not join them or write to stdout.
fn finish_bridge(outcome: shepr_platform::RemoteBridgeOutcome) -> CliResult<i32> {
    match outcome {
        shepr_platform::RemoteBridgeOutcome::Closed => Ok(0),
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
fn init_client_logging(paths: &shepr_config::AppPaths) -> io::Result<()> {
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

fn resolve_bridge_paths() -> CliResult<shepr_config::AppPaths> {
    shepr_config::AppPaths::resolve().map_err(|errors| {
        CliError::Io(io::Error::other(format!(
            "application paths could not be resolved: {}",
            errors.join("; ")
        )))
    })
}

fn load_validated_config(
    resolved_paths: Result<shepr_config::AppPaths, Vec<String>>,
) -> CliResult<shepr_config::ValidatedClientConfig> {
    let paths = resolved_paths.map_err(CliError::Config)?;
    shepr_config::load_client_validated(&paths).map_err(|diagnostics| {
        CliError::Config(diagnostics.iter().map(ToString::to_string).collect())
    })
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_shepr_blocks_when_env_is_set() {
        assert!(should_block_nested_for_env(Some(SHEPR_ENV_IN_PANE), None));
    }

    #[test]
    fn nested_shepr_blocks_with_a_matching_profile() {
        assert!(should_block_nested_for_env(
            Some(SHEPR_ENV_IN_PANE),
            Some(shepr_config::BuildProfile::current().marker())
        ));
    }

    #[test]
    fn nested_shepr_does_not_block_without_env() {
        assert!(!should_block_nested_for_env(None, None));
    }

    #[test]
    fn server_stop_does_not_load_a_broken_config() {
        let env = crate::test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("broken-launch-config");
        env.set(EnvVar::XdgConfigHome, scratch.path());
        let paths = shepr_config::AppPaths::resolve().expect("resolve paths");
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
            shepr_config::BuildProfile::current().marker(),
        );
        let paths = shepr_config::AppPaths::resolve().expect("resolve paths");
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
        let other_profile = match shepr_config::BuildProfile::current() {
            shepr_config::BuildProfile::Release => "dev",
            shepr_config::BuildProfile::Dev => "release",
        };
        assert!(!should_block_nested_for_env(
            Some(SHEPR_ENV_IN_PANE),
            Some(other_profile)
        ));
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
