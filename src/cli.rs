use clap::ArgMatches;

use shepr_api::client::{ApiClient, ApiClientError};
use shepr_api::schema::{Request, ResponseResult};
use shepr_launch::invocation::{
    COMMAND_CLIENT, COMMAND_DETECT, COMMAND_REMOTE_CLIENT_BRIDGE, COMMAND_SERVER, COMMAND_STATUS,
};

/// Writes CLI output to stdout, as `std::print!` does (a failed write
/// panics), unless a test has captured this thread's output.
fn write_cli_output(arguments: std::fmt::Arguments<'_>) {
    if output_capture::write(arguments) {
        return;
    }
    use std::io::Write as _;
    shepr_platform::begin_cli_output();
    if let Err(error) = std::io::stdout().lock().write_fmt(arguments) {
        panic!("write CLI output: {error}");
    }
}

macro_rules! print {
    ($($arg:tt)*) => {{
        $crate::cli::write_cli_output(format_args!($($arg)*));
    }};
}

macro_rules! println {
    () => {{
        $crate::cli::write_cli_output(format_args!("\n"));
    }};
    ($($arg:tt)*) => {{
        $crate::cli::write_cli_output(format_args!("{}\n", format_args!($($arg)*)));
    }};
}

mod detect;
mod error;
mod matches;
mod server;
mod spec;
mod status;

pub(crate) use error::{CliError, finish_client, print_notice};
pub(crate) type CliResult<T> = Result<T, CliError>;

/// A top-level command after clap has parsed argv once. Launch modes and CLI
/// command groups are explicit, and CLI groups already contain typed values.
pub(crate) enum Launch {
    Help,
    Version,
    Tui,
    Client,
    ClientBridge,
    Cli(Box<CliCommand>),
}

pub(crate) enum CliCommand {
    Status(status::Command),
    ClientStatus { json: bool },
    Server(server::Command),
    Detect(detect::Command),
}

impl CliCommand {
    fn from_matches(name: &str, matches: &ArgMatches) -> Option<Self> {
        Some(match name {
            COMMAND_STATUS => match status::parse(matches)? {
                status::ParsedCommand::Local(command) => Self::Status(command),
                status::ParsedCommand::Client { json } => Self::ClientStatus { json },
            },
            COMMAND_SERVER => Self::Server(server::parse(matches)?),
            COMMAND_DETECT => Self::Detect(detect::parse(matches)?),
            _ => return None,
        })
    }
}

/// Parses argv. On a usage error, or when `--help` for a subcommand was asked
/// for, clap's message has already been printed and the exit code is returned.
pub(crate) fn parse_launch(args: &[String]) -> Result<Launch, i32> {
    if let Some(launch) = root_exit_flags_before_subcommand(args) {
        return Ok(launch);
    }

    match spec::command().try_get_matches_from(args) {
        Ok(matches) => {
            let launch = match matches.subcommand() {
                None => Launch::Tui,
                Some((COMMAND_CLIENT, _)) => Launch::Client,
                Some((COMMAND_REMOTE_CLIENT_BRIDGE, _)) => Launch::ClientBridge,
                Some((name, matches)) => match CliCommand::from_matches(name, matches) {
                    Some(command) => Launch::Cli(Box::new(command)),
                    // The CLI spec and typed parsers are checked together in
                    // tests; retain a clear error in release builds if they diverge.
                    None => {
                        shepr_platform::begin_cli_output();
                        eprintln!(
                            "error: command '{name}' does not match a typed parser; run with --help for usage"
                        );
                        return Err(2);
                    }
                },
            };
            if matches::flag(&matches, "help") {
                Ok(Launch::Help)
            } else if matches::flag(&matches, "version") {
                Ok(Launch::Version)
            } else {
                Ok(launch)
            }
        }
        Err(error) => {
            shepr_platform::begin_cli_output();
            if let Err(print_error) = error.print() {
                std::eprintln!("error: {print_error}");
            }
            Err(error.exit_code())
        }
    }
}

/// Root help and version flags take precedence over validation of a following
/// subcommand, including required arguments nested below a command group.
fn root_exit_flags_before_subcommand(args: &[String]) -> Option<Launch> {
    let mut help = false;
    let mut version = false;
    let mut has_subcommand = false;

    let specification = spec::command();
    for argument in args.iter().skip(1).map(String::as_str) {
        match argument {
            "-h" | "--help" => help = true,
            "-V" | "--version" => version = true,
            name if specification
                .get_subcommands()
                .any(|command| command.get_name() == name) =>
            {
                has_subcommand = true;
                break;
            }
            _ => break,
        }
    }

    if !has_subcommand {
        return None;
    }
    if help {
        Some(Launch::Help)
    } else if version {
        Some(Launch::Version)
    } else {
        None
    }
}

pub(crate) fn print_help() {
    shepr_platform::begin_cli_output();
    let help = spec::command().render_help().to_string();
    print!("{help}");
    if !help.ends_with("\n\n") {
        println!();
    }
    match shepr_paths::AppPaths::resolve() {
        Ok(paths) => {
            println!("Client config: {}", paths.client_config_file().display());
            println!("Server config: {}", paths.server_config_file().display());
            println!(
                "Logs:          {}",
                shepr_platform::logging::help_log_paths_summary(paths.data_dir())
            );
        }
        Err(error) => {
            println!("Config:        unavailable ({error})");
            println!("Logs:          unavailable ({error})");
        }
    }
}

/// Runs one parsed CLI command. Launch modes are handled by `main` directly.
pub(crate) fn run(command: &CliCommand) -> CliResult<i32> {
    match command {
        CliCommand::Detect(detect::Command::Explain(detect::ExplainArgs {
            source: detect::ExplainSource::File { path, agent },
            json,
            verbose,
        })) => detect::run_file_explain(path, agent, *json, *verbose),
        CliCommand::ClientStatus { json } => {
            // This identity report reads only the binaries, never sockets or
            // runtime paths, so it works even where application paths cannot
            // be resolved.
            status::print_client_status(*json)?;
            Ok(0)
        }
        CliCommand::Status(command) => {
            run_with_paths(|paths| status::run_status_command(*command, paths))
        }
        CliCommand::Server(command) => {
            run_with_paths(|paths| server::run_server_command(command.clone(), paths))
        }
        CliCommand::Detect(command) => {
            run_with_paths(|paths| detect::run_detect_command(command.clone(), paths))
        }
    }
}

fn run_with_paths(run: impl FnOnce(&shepr_paths::AppPaths) -> CliResult<i32>) -> CliResult<i32> {
    let paths = resolve_app_paths()?;
    run(&paths)
}

fn resolve_app_paths() -> CliResult<shepr_paths::AppPaths> {
    shepr_paths::AppPaths::resolve().map_err(CliError::from)
}

/// Sends one request to the local server and decodes the response envelope
/// through shepr-api's schema: the typed result, or the server's error
/// response as [`CliError::Response`]. Commands match on the result variant
/// they asked for and never probe the JSON.
fn send_request(paths: &shepr_paths::AppPaths, request: &Request) -> CliResult<ResponseResult> {
    let client = ApiClient::local(paths);
    ensure_server_build_matches(paths, &client, &request.id)?;
    client
        .request(request)
        .map(|success| success.result)
        .map_err(|err| map_server_not_running_or_io(paths, err, &request.id, &client))
}

/// The failure for a successful response whose result is not the variant the
/// command asked for.
fn unexpected_result(result: &ResponseResult) -> CliError {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("unexpected api result: {result:?}"),
    )
    .into()
}

fn ensure_server_build_matches(
    paths: &shepr_paths::AppPaths,
    client: &ApiClient,
    request_id: &str,
) -> CliResult<()> {
    let pong = client
        .ping()
        .map_err(|err| map_server_not_running_or_io(paths, err, request_id, client))?;
    if pong.build_id.is_this_build() {
        return Ok(());
    }
    let response = shepr_api::schema::ErrorResponse {
        id: Some(request_id.to_owned()),
        error: shepr_api::schema::ErrorBody::new(
            &shepr_api::error::ApiErrorCode::BuildMismatch,
            format!(
                "this shepr client (build {}) differs from the running server (build {}); restart the server with this build before using this command. {}",
                shepr_protocol::BUILD_ID,
                pong.build_id,
                shepr_launch::guidance::build_mismatch_guidance(paths.server_address())
            ),
        ),
    };

    Err(CliError::Response(response))
}

/// Whether the local server socket is definitely absent or stale. Other probe
/// failures remain transport errors because they do not establish liveness.
pub(super) fn server_not_running_error(socket_path: &std::path::Path) -> CliResult<bool> {
    shepr_platform::ipc::socket_is_live(socket_path)
        .map(|live| !live)
        .map_err(Into::into)
}

/// Classify a socket failure before it reaches the CLI printer.
fn map_server_not_running_or_io(
    paths: &shepr_paths::AppPaths,
    err: ApiClientError,
    request_id: &str,
    client: &ApiClient,
) -> CliError {
    match err {
        ApiClientError::Io(_)
            if server_not_running_error(&client.socket_path()).unwrap_or(false) =>
        {
            let socket_path = client.socket_path();
            let attach_command = shepr_launch::guidance::attach_command(paths.server_address());
            let message = shepr_launch::guidance::server_not_running(&socket_path, &attach_command);
            CliError::Response(shepr_api::schema::ErrorResponse {
                id: Some(request_id.to_owned()),
                error: shepr_api::schema::ErrorBody::new(
                    &shepr_api::error::ApiErrorCode::ServerNotRunning,
                    message,
                ),
            })
        }
        ApiClientError::ErrorResponse(response) => CliError::Response(response),
        ApiClientError::Io(err) => err.into(),
        err @ (ApiClientError::Json(_) | ApiClientError::EmptyResponse) => {
            std::io::Error::new(std::io::ErrorKind::InvalidData, err).into()
        }
        err @ ApiClientError::UnexpectedResult(_) => std::io::Error::other(err).into(),
    }
}

/// Outside tests, nothing captures CLI output.
#[cfg(not(test))]
mod output_capture {
    pub(super) fn write(_arguments: std::fmt::Arguments<'_>) -> bool {
        false
    }
}

/// Test capture of CLI output, per thread, so tests assert on what a command
/// prints instead of writing it into the test run's stdout.
#[cfg(test)]
mod output_capture {
    use std::cell::RefCell;
    use std::io::Write as _;
    use std::rc::Rc;

    type Buffer = Rc<RefCell<Vec<u8>>>;

    std::thread_local! {
        static BUFFER: RefCell<Option<Buffer>> = const { RefCell::new(None) };
    }

    pub(super) fn write(arguments: std::fmt::Arguments<'_>) -> bool {
        let Some(buffer) = BUFFER.with(|slot| slot.borrow().as_ref().map(Rc::clone)) else {
            return false;
        };
        buffer
            .borrow_mut()
            .write_fmt(arguments)
            .expect("write captured CLI output");
        true
    }

    /// Runs `run` with this thread's CLI output captured, and returns what it
    /// printed.
    pub(super) fn capture<T>(run: impl FnOnce() -> T) -> (T, Vec<u8>) {
        struct Restore(Option<Buffer>);

        impl Drop for Restore {
            fn drop(&mut self) {
                BUFFER.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }

        let buffer = Rc::new(RefCell::new(Vec::new()));
        let restore = BUFFER.with(|slot| Restore(slot.replace(Some(Rc::clone(&buffer)))));
        let result = run();
        drop(restore);
        let output = buffer.borrow().clone();
        (result, output)
    }
}

#[cfg(test)]
mod tests {
    use super::{CliCommand, CliError, Launch};
    use shepr_test_fixtures::*;

    pub(super) fn parse(args: &[&str]) -> Launch {
        let mut argv = vec!["shepr".to_string()];
        argv.extend(args.iter().map(ToString::to_string));
        match super::parse_launch(&argv) {
            Ok(invocation) => invocation,
            Err(code) => panic!("{args:?} should parse (exit {code})"),
        }
    }

    fn parse_error(args: &[&str]) -> clap::Error {
        let mut argv = vec!["shepr".to_string()];
        argv.extend(args.iter().map(ToString::to_string));
        match super::spec::command().try_get_matches_from(&argv) {
            Ok(_) => panic!("{args:?} should be rejected"),
            Err(error) => error,
        }
    }

    #[test]
    fn every_cli_spec_root_has_typed_parser() {
        let samples: [(&str, &[&str]); 3] = [
            ("status", &["status"]),
            ("server", &["server", "stop"]),
            ("detect", &["detect", "capture", "w1:p1"]),
        ];
        let launch_only = ["client", "remote-client-bridge"];
        let spec = super::spec::command();
        let mut spec_groups = spec
            .get_subcommands()
            .map(clap::Command::get_name)
            .filter(|name| !launch_only.contains(name))
            .collect::<Vec<_>>();
        let mut sampled_groups = samples.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        spec_groups.sort_unstable();
        sampled_groups.sort_unstable();
        assert_eq!(spec_groups, sampled_groups);

        for (_, args) in samples {
            let launch = parse(args);
            assert!(
                matches!(launch, Launch::Cli(_)),
                "{args:?} should produce a typed CLI command"
            );
        }
    }

    /// The matches for `shepr <group>` in a command invocation.
    pub(super) fn group_matches(args: &[&str]) -> clap::ArgMatches {
        let mut argv = vec!["shepr".to_string()];
        argv.extend(args.iter().map(ToString::to_string));
        let matches = super::spec::command()
            .try_get_matches_from(argv)
            .unwrap_or_else(|error| panic!("{args:?} should parse: {error}"));
        let Some((_, group)) = matches.subcommand() else {
            panic!("{args:?} is not a CLI command");
        };
        group.clone()
    }

    /// The sub-matches of `shepr <group> <command> ...`.
    pub(super) fn command_matches(args: &[&str]) -> clap::ArgMatches {
        let group = group_matches(args);
        let Some((_, command)) = group.subcommand() else {
            panic!("{args:?} has no subcommand");
        };
        command.clone()
    }

    /// A bare `server stop` is unconditional; the hidden `--expect-boot` makes it
    /// conditional on the named boot.
    #[test]
    fn server_stop_parses_the_expected_boot() {
        let expect_boot = shepr_launch::invocation::FLAG_EXPECT_BOOT;
        for (args, expected) in [
            (
                &["server", "stop", expect_boot, "4242-17"][..],
                Some("4242-17"),
            ),
            (&["server", "stop"][..], None),
        ] {
            let launch = parse(args);
            let Launch::Cli(command) = launch else {
                panic!("{args:?} should be a typed CLI command");
            };
            assert!(
                matches!(
                    &*command,
                    CliCommand::Server(super::server::Command::Stop { expected_boot })
                        if expected_boot.as_deref() == expected
                ),
                "{args:?}"
            );
        }
    }

    #[test]
    fn equals_form_works_for_every_value_option() {
        let explain =
            command_matches(&["detect", "explain", "--file=screen.txt", "--agent=claude"]);
        assert_eq!(
            super::matches::try_string(&explain, "file")
                .expect("test argument is a string")
                .as_deref(),
            Some("screen.txt")
        );
        assert_eq!(
            super::matches::try_string(&explain, "agent")
                .expect("test argument is a string")
                .as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn hidden_launch_modes_are_typed() {
        assert!(matches!(
            parse(&["remote-client-bridge"]),
            Launch::ClientBridge
        ));
        assert!(matches!(parse(&["client"]), Launch::Client));
        assert!(matches!(parse(&[]), Launch::Tui));
    }

    #[test]
    fn client_status_does_not_require_runtime_paths() {
        let env = crate::test_support::IsolatedEnv::new();
        env.remove(shepr_core::env::EnvVar::XdgRuntimeDir.name());
        let launch = parse(&["status", "client", "--json"]);
        let Launch::Cli(command) = launch else {
            panic!("status client should be a CLI command");
        };
        let (result, output) = super::output_capture::capture(|| super::run(&command));
        assert_eq!(result.expect("client status needs no paths"), 0);
        let output: serde_json::Value =
            serde_json::from_slice(&output).expect("client status writes JSON");
        assert_eq!(output["build_id"].as_str(), Some(shepr_protocol::BUILD_ID));
    }

    #[test]
    fn unknown_commands_and_launch_flags_are_rejected() {
        for args in [
            &["frobnicate"][..],
            &["--bogus"],
            &["server"],
            &["--session", "work"],
            &["--session=work", "server", "stop"],
            &["server", "stop", "--session=api"],
            &["server", "stop", "--force"],
            &["session", "list"],
            &["session", "stop", "work"],
            &["session", "attach", "work"],
            &["session", "delete", "work"],
            &["machine", "list"],
            &["machine", "status"],
            &["machine", "add", "host", "--label", "h"],
            &["machine", "remove", "h"],
            &["machine", "reconnect", "build"],
            &["api", "snapshot"],
            &["workspace", "list"],
            &["tab", "list"],
            &["pane", "list"],
            &["agent", "list"],
            &["terminal", "attach", "term_1_1"],
            &["terminal", "title", "clear"],
            &["config", "reset-keys"],
            &["config", "check"],
            &["config"],
            &["integration", "status"],
            &["integration", "install", "claude"],
            &["integration", "uninstall", "claude"],
            &["--remote", "host"],
            &["--remote", "host", "--remote-keybindings", "local"],
            &["--remote-keybindings", "server"],
            &["--default-config"],
            &["--machine", "mac", "status"],
            &["--machine=mac", "server", "stop"],
            &["status", "--machine", "mac"],
            &["remote-api-bridge"],
            &["remote-api-bridge", "--check"],
        ] {
            assert_eq!(parse_error(args).exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn maps_dead_server_connect_failure_to_friendly_error() {
        use shepr_api::client::{ApiClient, ApiClientError};

        let scratch = crate::test_support::ScratchDir::new("cli-socket-error");
        let paths = shepr_paths::AppPaths::test_at(scratch.path());
        let client = ApiClient::local(&paths);
        let socket = client.socket_path().display().to_string();

        // Classification carries the response to the final CLI printer.
        let mapped = super::map_server_not_running_or_io(
            &paths,
            ApiClientError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
            "cli:detect:capture",
            &client,
        );

        let CliError::Response(response) = &mapped else {
            panic!("dead-server connect failure should carry a response");
        };
        assert_eq!(response.id.as_deref(), Some("cli:detect:capture"));
        assert_eq!(
            response.error.code,
            shepr_api::error::ApiErrorCode::ServerNotRunning
        );
        assert!(response.error.message.contains(&socket));
    }

    #[test]
    fn classifier_ignores_unrelated_io_kinds() {
        use shepr_api::client::{ApiClient, ApiClientError};

        let scratch = crate::test_support::ScratchDir::new("cli-socket-classifier");
        let paths = shepr_paths::AppPaths::test_at(scratch.path());
        std::fs::create_dir_all(paths.runtime_dir()).expect("create test runtime directory");
        let client = ApiClient::local(&paths);
        let _listener = shepr_platform::ipc::bind_local_listener(&client.socket_path())
            .expect("bind test server socket");
        let mapped = super::map_server_not_running_or_io(
            &paths,
            ApiClientError::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)),
            "cli:detect:capture",
            &client,
        );
        assert!(!matches!(
            &mapped,
            CliError::Response(response)
                if response.error.code == shepr_api::error::ApiErrorCode::ServerNotRunning
        ));
    }
}
