use clap::ArgMatches;

use shepr_api::client::{ApiClient, ApiClientError};
use shepr_api::schema::Request;
use shepr_remote::{COMMAND_CLIENT, COMMAND_REMOTE_CLIENT_BRIDGE, COMMAND_SERVER, COMMAND_STATUS};

macro_rules! print {
    ($($arg:tt)*) => {{
        shepr_platform::begin_cli_output();
        std::print!($($arg)*);
    }};
}

macro_rules! println {
    ($($arg:tt)*) => {{
        shepr_platform::begin_cli_output();
        std::println!($($arg)*);
    }};
}

mod detect;
mod error;
mod integration;
mod matches;
mod server;
mod server_not_running;
mod spec;
mod status;
mod target;

pub(crate) use error::{CliError, finish_client, print_notice};
pub(crate) type CliResult<T> = Result<T, CliError>;

/// A top-level command after clap has parsed argv once. Launch modes and CLI
/// command groups are explicit, and CLI groups already contain typed values.
pub(crate) enum Launch {
    Tui,
    HeadlessServer,
    Client,
    ClientBridge,
    Cli(Box<CliCommand>),
}

pub(crate) enum CliCommand {
    Status(status::Command),
    Server(server::Command),
    Detect(detect::Command),
    Integration(integration::Command),
}

impl CliCommand {
    fn from_matches(name: &str, matches: &ArgMatches) -> Option<Self> {
        Some(match name {
            COMMAND_STATUS => Self::Status(status::parse(matches)?),
            COMMAND_SERVER => Self::Server(server::parse(matches)?),
            "detect" => Self::Detect(detect::parse(matches)?),
            "integration" => Self::Integration(integration::parse(matches)?),
            _ => return None,
        })
    }
}

/// Values parsed from the root options plus one typed launch. Launch options
/// are only recognised before the subcommand; everything after it belongs to
/// that subcommand.
pub(crate) struct Invocation {
    pub(crate) launch: Launch,
    help: bool,
    version: bool,
}

/// Parses argv. On a usage error, or when `--help` for a subcommand was asked
/// for, clap's message has already been printed and the exit code is returned.
pub(crate) fn parse_invocation(args: &[String]) -> Result<Invocation, i32> {
    match spec::command().try_get_matches_from(args) {
        Ok(matches) => {
            let launch = match matches.subcommand() {
                None => Launch::Tui,
                Some((COMMAND_SERVER, matches)) if matches.subcommand().is_none() => {
                    Launch::HeadlessServer
                }
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
            Ok(Invocation {
                launch,
                help: matches::flag(&matches, "help"),
                version: matches::flag(&matches, "version"),
            })
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

impl Invocation {
    pub(crate) fn help_requested(&self) -> bool {
        self.help
    }

    pub(crate) fn version_requested(&self) -> bool {
        self.version
    }

    /// The CLI command this invocation runs, or `None` for a launch mode
    /// (TUI, server, client) that is not a CLI command.
    pub(crate) fn cli_command(&self) -> Option<&CliCommand> {
        match &self.launch {
            Launch::Cli(command) => Some(command.as_ref()),
            _ => None,
        }
    }
}

pub(crate) fn print_help() {
    shepr_platform::begin_cli_output();
    let help = spec::command().render_help().to_string();
    print!("{help}");
    if !help.ends_with("\n\n") {
        println!();
    }
    match shepr_config::AppPaths::resolve() {
        Ok(paths) => {
            println!("Config: {}", paths.config_file().display());
            println!(
                "Logs:   {}",
                shepr_platform::logging::help_log_paths_summary(paths.data_dir())
            );
        }
        Err(errors) => {
            println!("Config: unavailable ({})", errors.join("; "));
            println!("Logs:   unavailable ({})", errors.join("; "));
        }
    }
    println!(
        "Env:    {} overrides config file path",
        shepr_core::env::EnvVar::SheprConfigPath
    );
}

/// Runs one parsed CLI command. Launch modes are handled by `main` directly.
pub(crate) fn run(command: &CliCommand) -> CliResult<i32> {
    let paths = resolve_app_paths()?;
    let context = target::CliContext::local(paths);
    dispatch(command, &context)
}

fn dispatch(command: &CliCommand, context: &target::CliContext) -> CliResult<i32> {
    match command {
        CliCommand::Status(command) => status::run_status_command(*command, context),
        CliCommand::Server(command) => server::run_server_command(*command, context),
        CliCommand::Detect(command) => detect::run_detect_command(command.clone(), context),
        CliCommand::Integration(command) => {
            integration::run_integration_command(command.clone(), context)
        }
    }
}

fn resolve_app_paths() -> CliResult<shepr_config::AppPaths> {
    shepr_config::AppPaths::resolve().map_err(|diagnostics| {
        CliError::Io(std::io::Error::other(format!(
            "application paths could not be resolved:\n  {}",
            diagnostics.join("\n  ")
        )))
    })
}

fn send_request(context: &target::CliContext, request: &Request) -> CliResult<serde_json::Value> {
    let client = target::api_client(context);
    ensure_server_build_matches(context, &client, &request.id)?;
    client
        .request_value(request)
        .map_err(|err| map_server_not_running_or_io(context, err, &request.id, &client))
}

fn ensure_server_build_matches(
    context: &target::CliContext,
    client: &ApiClient,
    request_id: &str,
) -> CliResult<()> {
    // Checked once per target so polling commands need only one status request.
    if context.build_checked() {
        return Ok(());
    }
    let status = client
        .status()
        .map_err(|err| map_server_not_running_or_io(context, err, request_id, client))?;
    if shepr_protocol::is_this_build(&status.build_id) {
        context.mark_build_checked();
        return Ok(());
    }
    let response = shepr_api::schema::ErrorResponse {
        id: request_id.to_owned(),
        error: shepr_api::schema::ErrorBody::new(
            &shepr_api::error::ApiErrorCode::BuildMismatch,
            format!(
                "this shepr client (build {}) differs from the running server (build {}); restart the server with this build before using this command. {}",
                shepr_protocol::BUILD_ID,
                status.build_id,
                target::restart_guidance(context)
            ),
        ),
    };

    Err(CliError::Response(response))
}

/// Whether the local API socket is definitely absent or stale. Other probe
/// failures remain transport errors because they do not establish liveness.
pub(super) fn server_not_running_error(socket_path: &std::path::Path) -> CliResult<bool> {
    match shepr_platform::ipc::probe(socket_path) {
        shepr_platform::ipc::Liveness::Absent | shepr_platform::ipc::Liveness::Stale => Ok(true),
        shepr_platform::ipc::Liveness::Live => Ok(false),
        shepr_platform::ipc::Liveness::Unreachable(error) => Err(error.into()),
    }
}

/// Classify a socket failure before it reaches the CLI printer.
fn map_server_not_running_or_io(
    context: &target::CliContext,
    err: ApiClientError,
    request_id: &str,
    client: &ApiClient,
) -> CliError {
    match err {
        ApiClientError::Io(_)
            if server_not_running_error(&client.socket_path()).unwrap_or(false) =>
        {
            server_not_running::cli_error(server_not_running::response(
                request_id,
                &client.socket_path(),
                context,
            ))
        }
        err => api_client_error_to_io(err).into(),
    }
}

fn api_client_error_to_io(err: ApiClientError) -> std::io::Error {
    match err {
        ApiClientError::Io(err) => err,
        err => std::io::Error::other(err),
    }
}

#[cfg(test)]
mod tests {
    use super::{CliCommand, Invocation, Launch};
    use shepr_test_fixtures::*;

    pub(super) fn parse(args: &[&str]) -> Invocation {
        let mut argv = vec!["shepr".to_string()];
        argv.extend(args.iter().map(ToString::to_string));
        match super::parse_invocation(&argv) {
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
        let samples: [(&str, &[&str]); 4] = [
            ("status", &["status"]),
            ("server", &["server", "stop"]),
            ("detect", &["detect", "capture", "w1:p1"]),
            ("integration", &["integration", "status"]),
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
            let invocation = parse(args);
            assert!(
                matches!(invocation.launch, Launch::Cli(_)),
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

    /// `--force` is the stated intent to stop a server of another build, and
    /// it is spelled as the refusal names it.
    #[test]
    fn server_stop_parses_the_force_flag() {
        let force = shepr_api::session::FORCE_STOP_FLAG;
        for (args, forced) in [
            (&["server", "stop", force][..], true),
            (&["server", "stop"][..], false),
        ] {
            let invocation = parse(args);
            let Launch::Cli(command) = invocation.launch else {
                panic!("{args:?} should be a typed CLI command");
            };
            assert!(
                matches!(
                    *command,
                    CliCommand::Server(super::server::Command::Stop { force }) if force == forced
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
            super::matches::string(&explain, "file").as_deref(),
            Some("screen.txt")
        );
        assert_eq!(
            super::matches::string(&explain, "agent").as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn hidden_launch_modes_are_typed() {
        assert!(matches!(
            parse(&["remote-client-bridge"]).launch,
            Launch::ClientBridge
        ));
        assert!(matches!(parse(&["client"]).launch, Launch::Client));
        assert!(matches!(parse(&[]).launch, Launch::Tui));
    }

    #[test]
    fn unknown_commands_and_launch_flags_are_rejected() {
        for args in [
            &["frobnicate"][..],
            &["--bogus"],
            &["--session", "work"],
            &["--session=work", "server", "stop"],
            &["server", "stop", "--session=api"],
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
        let paths =
            super::target::CliContext::test_local(shepr_config::AppPaths::test_at(scratch.path()));
        let client = ApiClient::local(&paths);
        let socket = client.socket_path().display().to_string();

        // Classification carries the response to the final CLI printer.
        let mapped = super::map_server_not_running_or_io(
            &paths,
            ApiClientError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
            "cli:detect:capture",
            &client,
        );

        let response = super::server_not_running::reported_response(&mapped)
            .expect("dead-server connect failure should carry a server_not_running response");
        assert_eq!(response.id, "cli:detect:capture");
        assert_eq!(
            response.error.code,
            shepr_api::error::ApiErrorCode::ServerNotRunning.as_str()
        );
        assert!(response.error.message.contains(&socket));

        // The API error code is checked through its canonical constant.
        assert!(super::server_not_running::was_reported(&mapped));
    }

    #[test]
    fn classifier_ignores_unrelated_io_kinds() {
        use shepr_api::client::{ApiClient, ApiClientError};

        let scratch = crate::test_support::ScratchDir::new("cli-socket-classifier");
        let paths =
            super::target::CliContext::test_local(shepr_config::AppPaths::test_at(scratch.path()));
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
        assert!(!super::server_not_running::was_reported(&mapped));
    }
}
