use clap::ArgMatches;

use shepr_api::client::{ApiClient, ApiClientError};
use shepr_api::schema::Request;
use shepr_remote::{
    COMMAND_CLIENT, COMMAND_REMOTE_API_BRIDGE, COMMAND_REMOTE_CLIENT_BRIDGE, COMMAND_SERVER,
    COMMAND_STATUS, COMMAND_STOP, FLAG_CHECK, FLAG_SESSION, option_name_from_flag,
};

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
mod machine;
mod matches;
pub(crate) mod operator;
mod server;
mod server_not_running;
mod spec;
mod status;
mod target;

use error::SessionCliError;
pub(crate) use error::{CliError, finish_client, print_notice};
pub(crate) type CliResult<T> = Result<T, CliError>;

/// A top-level command after clap has parsed argv once. Launch modes and CLI
/// command groups are explicit, and CLI groups already contain typed values.
pub(crate) enum Launch {
    Tui { attached_session: Option<String> },
    HeadlessServer,
    Client,
    ApiBridge { check: bool },
    ClientBridge,
    Cli(Box<CliCommand>),
}

pub(crate) enum CliCommand {
    Status(status::Command),
    Machine(machine::Command),
    Server(server::Command),
    Detect(detect::Command),
    Session(SessionCommand),
    Integration(integration::Command),
}

impl CliCommand {
    fn from_matches(name: &str, matches: &ArgMatches) -> Option<Self> {
        Some(match name {
            COMMAND_STATUS => Self::Status(status::parse(matches)?),
            "machine" => Self::Machine(machine::parse(matches)?),
            COMMAND_SERVER => Self::Server(server::parse(matches)?),
            "detect" => Self::Detect(detect::parse(matches)?),
            "session" => Self::Session(SessionCommand::parse(matches)?),
            "integration" => Self::Integration(integration::parse(matches)?),
            _ => return None,
        })
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Status(_) => COMMAND_STATUS,
            Self::Machine(_) => "machine",
            Self::Server(_) => COMMAND_SERVER,
            Self::Detect(_) => "detect",
            Self::Session(_) => "session",
            Self::Integration(_) => "integration",
        }
    }

    /// A command group may represent a valid invocation with no nested
    /// command, such as the `status` overview; absent names use `None`.
    pub(crate) fn subcommand_name(&self) -> Option<&'static str> {
        match self {
            Self::Status(command) => command.name(),
            Self::Machine(command) => Some(command.name()),
            Self::Server(command) => Some(command.name()),
            Self::Detect(command) => Some(command.name()),
            Self::Session(command) => Some(command.name()),
            Self::Integration(command) => Some(command.name()),
        }
    }

    pub(crate) fn can_run_on_machine(&self) -> bool {
        match self {
            Self::Status(command) => command.can_run_on_machine(),
            Self::Machine(command) => command.can_run_on_machine(),
            Self::Server(command) => command.can_run_on_machine(),
            Self::Detect(command) => command.can_run_on_machine(),
            Self::Session(command) => command.can_run_on_machine(),
            Self::Integration(command) => command.can_run_on_machine(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionCommand {
    List {
        json: bool,
    },
    Stop {
        name: String,
        json: bool,
        force: bool,
    },
    Delete {
        name: String,
        json: bool,
    },
}

impl SessionCommand {
    fn parse(matches: &ArgMatches) -> Option<Self> {
        match matches.subcommand() {
            Some(("list", command)) => Some(Self::List {
                json: matches::flag(command, "json"),
            }),
            Some((COMMAND_STOP, command)) => Some(Self::Stop {
                name: matches::required(command, "name")?,
                json: matches::flag(command, "json"),
                force: matches::flag(command, "force"),
            }),
            Some(("delete", command)) => Some(Self::Delete {
                name: matches::required(command, "name")?,
                json: matches::flag(command, "json"),
            }),
            // `session attach` is a launch mode and is handled above.
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Stop { .. } => COMMAND_STOP,
            Self::Delete { .. } => "delete",
        }
    }

    fn can_run_on_machine(&self) -> bool {
        match self {
            Self::List { .. } | Self::Stop { .. } | Self::Delete { .. } => false,
        }
    }
}

/// Values parsed from the root options plus one typed launch. Launch options
/// are only recognised before the subcommand; everything after it belongs to
/// that subcommand.
pub(crate) struct Invocation {
    pub(crate) launch: Launch,
    session: Option<String>,
    machine: Option<String>,
    help: bool,
    version: bool,
}

/// Parses argv. On a usage error, or when `--help` for a subcommand was asked
/// for, clap's message has already been printed and the exit code is returned.
pub(crate) fn parse_invocation(args: &[String]) -> Result<Invocation, i32> {
    match spec::command().try_get_matches_from(args) {
        Ok(matches) => {
            let launch = match matches.subcommand() {
                None => Launch::Tui {
                    attached_session: None,
                },
                Some((COMMAND_SERVER, matches)) if matches.subcommand().is_none() => {
                    Launch::HeadlessServer
                }
                Some((COMMAND_CLIENT, _)) => Launch::Client,
                Some((COMMAND_REMOTE_API_BRIDGE, matches)) => Launch::ApiBridge {
                    check: matches::flag(matches, option_name_from_flag(FLAG_CHECK)),
                },
                Some((COMMAND_REMOTE_CLIENT_BRIDGE, _)) => Launch::ClientBridge,
                Some(("session", matches))
                    if matches
                        .subcommand()
                        .is_some_and(|(name, _)| name == "attach") =>
                {
                    let attached_session = matches
                        .subcommand()
                        .and_then(|(_, attach)| matches::string(attach, "name"));
                    Launch::Tui { attached_session }
                }
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
                session: matches::string(&matches, option_name_from_flag(FLAG_SESSION)),
                machine: matches::string(&matches, "machine"),
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
    pub(crate) fn session(&self) -> Option<String> {
        self.session.clone()
    }

    pub(crate) fn machine(&self) -> Option<String> {
        self.machine.clone()
    }

    pub(crate) fn help_requested(&self) -> bool {
        self.help
    }

    pub(crate) fn version_requested(&self) -> bool {
        self.version
    }

    /// The name given to `session attach NAME`. That command is the default
    /// launch into the named session, the same as `--session NAME`.
    pub(crate) fn session_attach_name(&self) -> Option<String> {
        match &self.launch {
            Launch::Tui {
                attached_session: Some(name),
            } => Some(name.clone()),
            _ => None,
        }
    }

    /// The session this invocation explicitly targets, if any.
    pub(crate) fn requested_session(&self) -> Result<Option<String>, String> {
        match (self.session(), self.session_attach_name()) {
            (Some(_), Some(_)) => Err(
                "--session cannot be combined with `session attach`; name the session once".into(),
            ),
            (session, attach) => Ok(session.or(attach)),
        }
    }
}

impl Invocation {
    /// The CLI command this invocation runs, or `None` for a launch mode
    /// (TUI, server, client) that is not a CLI command.
    pub(crate) fn cli_command(&self) -> Option<&CliCommand> {
        match &self.launch {
            Launch::Cli(command) => Some(command.as_ref()),
            _ => None,
        }
    }
}

pub(crate) fn print_help(requested_session: Option<shepr_config::SessionId>) {
    shepr_platform::begin_cli_output();
    let help = spec::command().render_help().to_string();
    print!("{help}");
    if !help.ends_with("\n\n") {
        println!();
    }
    match shepr_config::AppPaths::resolve_with_session(requested_session) {
        Ok(paths) => {
            println!("Config: {}", paths.config_file().display());
            println!(
                "Logs:   {}",
                shepr_platform::logging::help_log_paths_summary(&shepr_api::session::data_dir(
                    &paths
                ))
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

/// Runs the invocation's subcommand against the saved machine named by
/// `--machine`.
pub(crate) fn run_on_machine(command: Option<&CliCommand>, selector: &str) -> CliResult<i32> {
    let paths = resolve_machine_app_paths()?;
    target::run_on_machine(selector, command, &paths)
}

/// Runs one parsed CLI command. Launch modes are handled by `main` directly.
pub(crate) fn run(
    command: &CliCommand,
    requested_session: Option<shepr_config::SessionId>,
) -> CliResult<i32> {
    let paths = resolve_app_paths(requested_session)?;
    let context = target::CliContext::local(paths);
    dispatch(command, &context)
}

fn dispatch(command: &CliCommand, context: &target::CliContext) -> CliResult<i32> {
    match command {
        CliCommand::Status(command) => status::run_status_command(*command, context),
        CliCommand::Machine(command) => machine::run_machine_command(command.clone(), context),
        CliCommand::Server(command) => server::run_server_command(*command, context),
        CliCommand::Detect(command) => detect::run_detect_command(command.clone(), context),
        CliCommand::Session(command) => run_session_command(command.clone(), context),
        CliCommand::Integration(command) => {
            integration::run_integration_command(command.clone(), context)
        }
    }
}

fn resolve_app_paths(
    requested_session: Option<shepr_config::SessionId>,
) -> CliResult<shepr_config::AppPaths> {
    shepr_config::AppPaths::resolve_with_session(requested_session).map_err(|diagnostics| {
        CliError::Io(std::io::Error::other(format!(
            "application paths could not be resolved:\n  {}",
            diagnostics.join("\n  ")
        )))
    })
}

fn resolve_machine_app_paths() -> CliResult<shepr_config::AppPaths> {
    shepr_config::AppPaths::resolve_for_machine().map_err(|diagnostics| {
        CliError::Io(std::io::Error::other(format!(
            "application paths could not be resolved:\n  {}",
            diagnostics.join("\n  ")
        )))
    })
}

fn load_validated_config(
    paths: &shepr_config::AppPaths,
) -> CliResult<shepr_config::ValidatedConfig> {
    shepr_config::load_validated(paths).map_err(|diagnostics| {
        CliError::Io(std::io::Error::other(format!(
            "configuration error:\n  {}",
            diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n  ")
        )))
    })
}

fn run_session_command(command: SessionCommand, paths: &target::CliContext) -> CliResult<i32> {
    match command {
        SessionCommand::List { json } => session_list(paths, json),
        SessionCommand::Stop { name, json, force } => session_stop(&name, json, force, paths),
        SessionCommand::Delete { name, json } => session_delete(&name, json, paths),
    }
}

fn session_list(paths: &shepr_config::AppPaths, json: bool) -> CliResult<i32> {
    let sessions = shepr_api::session::list_sessions(paths)?;
    if json {
        let sessions = sessions
            .iter()
            .map(SessionInfoJson::from)
            .collect::<Vec<_>>();
        print_json(&serde_json::json!({
            "sessions": sessions,
        }));
    } else {
        print_session_table(&sessions);
    }
    Ok(0)
}

/// Deliberately skips the build check that `send_request` does: the
/// build-mismatch error tells the user to run `session stop` / `server
/// stop`, so stopping must keep working against a server from another build.
/// `shepr_api::session` sends a bare `server.stop` JSON line for that reason,
/// and refuses a server of another build unless `force` states the intent.
fn session_stop(
    name: &str,
    json: bool,
    force: bool,
    paths: &shepr_config::AppPaths,
) -> CliResult<i32> {
    let target = shepr_api::session::parse_target_name(name)
        .map_err(|message| CliError::Session(SessionCliError::InvalidName(message)))?;
    match shepr_api::session::stop_session(paths, &target, force) {
        Ok(session) => {
            if json {
                print_json(&serde_json::json!({
                    "stopped": true,
                    "session": SessionInfoJson::from(&session),
                }));
            } else {
                println!("stopped session {}", session.name);
            }
            Ok(0)
        }
        Err(error) => Err(CliError::Session(SessionCliError::Stop(error))),
    }
}

fn session_delete(name: &str, json: bool, paths: &shepr_config::AppPaths) -> CliResult<i32> {
    let target = shepr_api::session::parse_target_name(name)
        .map_err(|message| CliError::Session(SessionCliError::InvalidName(message)))?;
    match shepr_api::session::delete_session(paths, &target) {
        Ok(session) => {
            if json {
                print_json(&serde_json::json!({
                    "deleted": true,
                    "session": SessionInfoJson::from(&session),
                }));
            } else {
                println!("deleted session {}", session.name);
            }
            Ok(0)
        }
        Err(error) => Err(CliError::Session(SessionCliError::Delete(error))),
    }
}

fn print_response_error(response: &serde_json::Value) -> CliResult<bool> {
    if response.get("error").is_none() {
        return Ok(false);
    }
    eprintln!(
        "{}",
        serde_json::to_string(response).map_err(std::io::Error::other)?
    );
    Ok(true)
}

fn send_request(context: &target::CliContext, request: &Request) -> CliResult<serde_json::Value> {
    let client = target::api_client(context)?;
    ensure_server_build_matches(context, &client, &request.id)?;
    client
        .request_value(request)
        .map_err(|err| map_server_not_running_or_io(context, err, &request.id, &client))
}

fn send_request_unchecked(
    context: &target::CliContext,
    request: &Request,
) -> CliResult<serde_json::Value> {
    let client = target::api_client(context)?;
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
    let status = target::server_status(context, client)
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
    if context.is_remote() {
        return target::remote_error(context, api_client_error_to_io(err)).into();
    }
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

fn print_session_table(sessions: &[shepr_api::session::SessionInfo]) {
    println!(
        "{name:<name_width$} {status:<status_width$} {directory:<directory_width$} socket",
        name = "name",
        status = "status",
        directory = "directory",
        name_width = crate::limits::SESSION_TABLE_NAME_WIDTH,
        status_width = crate::limits::SESSION_TABLE_STATUS_WIDTH,
        directory_width = crate::limits::SESSION_TABLE_DIRECTORY_WIDTH,
    );
    for session in sessions {
        println!(
            "{name:<name_width$} {status:<status_width$} {directory:<directory_width$} {socket}",
            name = session.name,
            status = if session.running {
                "running"
            } else {
                "stopped"
            },
            directory = session.session_dir.display(),
            socket = session.socket_path.display(),
            name_width = crate::limits::SESSION_TABLE_NAME_WIDTH,
            status_width = crate::limits::SESSION_TABLE_STATUS_WIDTH,
            directory_width = crate::limits::SESSION_TABLE_DIRECTORY_WIDTH,
        );
    }
}

#[derive(serde::Serialize)]
struct SessionInfoJson<'a> {
    name: &'a str,
    default: bool,
    running: bool,
    socket_path: String,
    session_dir: String,
}

impl<'a> From<&'a shepr_api::session::SessionInfo> for SessionInfoJson<'a> {
    fn from(info: &'a shepr_api::session::SessionInfo) -> Self {
        Self {
            name: &info.name,
            default: info.default,
            running: info.running,
            socket_path: info.socket_path.display().to_string(),
            session_dir: info.session_dir.display().to_string(),
        }
    }
}

fn print_json(value: &serde_json::Value) {
    println!("{value}");
}

#[cfg(test)]
mod tests {
    use super::{CliCommand, Invocation, Launch, SessionCommand};
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
        let samples: [(&str, &[&str]); 6] = [
            ("status", &["status"]),
            ("machine", &["machine", "list"]),
            ("server", &["server", "stop"]),
            ("detect", &["detect", "capture", "w1:p1"]),
            ("session", &["session", "list"]),
            ("integration", &["integration", "status"]),
        ];
        let launch_only = ["client", "remote-api-bridge", "remote-client-bridge"];
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

        for (name, args) in samples {
            let invocation = parse(args);
            match invocation.launch {
                Launch::Cli(command) => assert_eq!(command.name(), name, "{args:?}"),
                _ => panic!("{args:?} should produce a typed CLI command"),
            }
        }
    }

    /// The matches for `shepr <group>` in a command invocation.
    pub(super) fn group_matches(args: &[&str]) -> clap::ArgMatches {
        let mut argv = vec!["shepr".to_string()];
        argv.extend(args.iter().map(ToString::to_string));
        let matches = super::spec::command()
            .try_get_matches_from(argv)
            .unwrap_or_else(|error| panic!("{args:?} should parse: {error}"));
        let Some((group_name, group)) = matches.subcommand() else {
            panic!("{args:?} is not a CLI command");
        };
        if group_name == "session" && group.subcommand_name() == Some("attach") {
            panic!("{args:?} is a launch mode, not a CLI command");
        }
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

    #[test]
    fn launch_options_are_read_before_the_subcommand() {
        let invocation = parse(&["--session", "work", "session", "list"]);
        assert_eq!(invocation.session().as_deref(), Some("work"));
        assert!(matches!(
            invocation.launch,
            Launch::Cli(command) if matches!(*command, CliCommand::Session(_))
        ));

        let invocation = parse(&["--session=api", "server", "stop"]);
        assert_eq!(invocation.session().as_deref(), Some("api"));
        assert!(matches!(
            invocation.launch,
            Launch::Cli(command) if matches!(*command, CliCommand::Server(_))
        ));
    }

    #[test]
    fn launch_options_after_the_subcommand_are_not_launch_options() {
        // A trailing launch option is a usage error, not a silent
        // retarget of the command.
        for args in [
            &["server", "stop", "--session=api"][..],
            &["session", "list", "--session", "work"],
            &["status", "--remote", "host"],
            &["detect", "capture", "w1:p1", "--machine", "mac"],
        ] {
            assert_eq!(parse_error(args).exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn session_command_arguments_are_typed_before_dispatch() {
        let invocation = parse(&["session", "stop", "--json", "--", "-work"]);
        let Launch::Cli(command) = invocation.launch else {
            panic!("session stop should be a typed CLI command");
        };
        let CliCommand::Session(SessionCommand::Stop { name, json, force }) = *command else {
            panic!("session stop should be a typed CLI command");
        };
        assert_eq!(name, "-work");
        assert!(json);
        assert!(!force);
    }

    /// `--force` is the stated intent to stop a server of another build, on
    /// both stop commands, and it is spelled as the refusal names it.
    #[test]
    fn both_stop_commands_parse_the_force_flag() {
        let force = shepr_api::session::FORCE_STOP_FLAG;
        let invocation = parse(&["session", "stop", force, "work"]);
        let Launch::Cli(command) = invocation.launch else {
            panic!("session stop should be a typed CLI command");
        };
        assert!(matches!(
            *command,
            CliCommand::Session(SessionCommand::Stop { force: true, .. })
        ));

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
    fn session_attach_is_a_launch_into_the_named_session() {
        let invocation = parse(&["session", "attach", "work"]);
        assert_eq!(invocation.session_attach_name().as_deref(), Some("work"));
        assert_eq!(
            invocation.requested_session().expect("test precondition"),
            Some("work".to_string())
        );
        assert!(matches!(invocation.launch, Launch::Tui { .. }));

        let invocation = parse(&["--session", "a", "session", "attach", "b"]);
        assert!(invocation.requested_session().is_err());

        assert_eq!(
            parse_error(&["session", "attach", "-h"]).kind(),
            clap::error::ErrorKind::DisplayHelp
        );
        assert_eq!(parse_error(&["session", "attach", "a", "b"]).exit_code(), 2);
    }

    #[test]
    fn session_name_accepts_option_terminator() {
        for name in ["-h", "--json"] {
            let stop = command_matches(&["session", "stop", "--", name]);
            assert_eq!(
                super::matches::required(&stop, "name").as_deref(),
                Some(name)
            );
            assert!(!super::matches::flag(&stop, "json"));
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
    fn hidden_launch_modes_keep_typed_options() {
        assert!(matches!(
            parse(&["--session", "work", "remote-client-bridge"]).launch,
            Launch::ClientBridge
        ));
        assert!(matches!(
            parse(&["remote-api-bridge", "--check"]).launch,
            Launch::ApiBridge { check: true }
        ));
        assert!(matches!(
            parse(&["remote-api-bridge"]).launch,
            Launch::ApiBridge { check: false }
        ));
    }

    #[test]
    fn unknown_commands_and_launch_flags_are_rejected() {
        for args in [
            &["frobnicate"][..],
            &["--bogus"],
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
    fn session_json_formats_paths_at_the_cli_edge() {
        let info = shepr_api::session::SessionInfo {
            name: "work".into(),
            default: false,
            running: true,
            socket_path: "/shepr-test/work/shepr.sock".into(),
            session_dir: "/shepr-test/work".into(),
        };
        let value = serde_json::to_value(super::SessionInfoJson::from(&info))
            .expect("session CLI JSON serializes");
        assert_eq!(value["socket_path"], "/shepr-test/work/shepr.sock");
        assert_eq!(value["session_dir"], "/shepr-test/work");
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
