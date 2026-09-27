use clap::ArgMatches;

use crate::api::client::{ApiClient, ApiClientError};
use crate::api::schema::{ClientWindowTitleSetParams, EmptyParams, Method, Request};

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

mod agent;
mod error;
mod integration;
mod machine;
mod matches;
mod pane;
mod protocol_guard;
mod runtime;
mod server;
mod server_not_running;
mod spec;
mod status;
mod tab;
mod target;
mod workspace;

pub(crate) use error::CliError;
use error::SessionCliError;
pub(crate) type CliResult<T> = Result<T, CliError>;

pub(crate) fn parse_token_assignment(raw: &str) -> Result<(String, Option<String>), String> {
    let Some((key, value)) = raw.split_once('=') else {
        return Err("token must use NAME=VALUE".into());
    };
    if key.is_empty() {
        return Err("token name must not be empty".into());
    }
    Ok((key.to_string(), Some(value.to_string())))
}

pub(crate) fn parse_env_assignment(raw: &str) -> Result<(String, String), String> {
    let Some((key, value)) = raw.split_once('=') else {
        return Err("env must use KEY=VALUE".into());
    };
    if key.is_empty() {
        return Err("env key must not be empty".into());
    }
    if key.contains('\0') || value.contains('\0') {
        return Err("env must not contain NUL bytes".into());
    }
    Ok((key.to_string(), value.to_string()))
}

/// A top-level command after clap has parsed argv once. Launch modes and CLI
/// command groups are explicit, and CLI groups already contain typed values.
pub(crate) enum Launch {
    Tui { attached_session: Option<String> },
    HeadlessServer,
    Client,
    ApiBridge { check: bool },
    ClientBridge { idle_timeout_v1: bool },
    Cli(Box<CliCommand>),
}

pub(crate) enum CliCommand {
    Status(status::Command),
    Config(ConfigCommand),
    Machine(machine::Command),
    Server(server::Command),
    Workspace(workspace::Command),
    Tab(tab::Command),
    Agent(agent::Command),
    Pane(pane::Command),
    Terminal(TerminalCommand),
    Session(SessionCommand),
    Integration(integration::Command),
}

impl CliCommand {
    fn from_matches(name: &str, matches: &ArgMatches) -> Option<Self> {
        Some(match name {
            "status" => Self::Status(status::parse(matches)),
            "config" => Self::Config(ConfigCommand::parse(matches)),
            "machine" => Self::Machine(machine::parse(matches)),
            "server" => Self::Server(server::parse(matches)),
            "workspace" => Self::Workspace(workspace::parse(matches)),
            "tab" => Self::Tab(tab::parse(matches)),
            "agent" => Self::Agent(agent::parse(matches)),
            "pane" => Self::Pane(pane::parse(matches)),
            "terminal" => Self::Terminal(TerminalCommand::parse(matches)),
            "session" => Self::Session(SessionCommand::parse(matches)),
            "integration" => Self::Integration(integration::parse(matches)),
            _ => return None,
        })
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Status(_) => "status",
            Self::Config(_) => "config",
            Self::Machine(_) => "machine",
            Self::Server(_) => "server",
            Self::Workspace(_) => "workspace",
            Self::Tab(_) => "tab",
            Self::Agent(_) => "agent",
            Self::Pane(_) => "pane",
            Self::Terminal(_) => "terminal",
            Self::Session(_) => "session",
            Self::Integration(_) => "integration",
        }
    }

    pub(crate) fn subcommand_name(&self) -> &'static str {
        match self {
            Self::Status(command) => command.name(),
            Self::Config(command) => command.name(),
            Self::Machine(command) => command.name(),
            Self::Server(command) => command.name(),
            Self::Workspace(command) => command.name(),
            Self::Tab(command) => command.name(),
            Self::Agent(command) => command.name(),
            Self::Pane(command) => command.name(),
            Self::Terminal(command) => command.name(),
            Self::Session(command) => command.name(),
            Self::Integration(command) => command.name(),
        }
    }

    pub(crate) fn is_api_command(&self) -> bool {
        match self {
            Self::Status(command) => command.is_api_command(),
            Self::Config(_) | Self::Machine(_) | Self::Session(_) | Self::Integration(_) => false,
            Self::Server(command) => command.is_api_command(),
            Self::Workspace(command) => command.is_api_command(),
            Self::Tab(command) => command.is_api_command(),
            Self::Pane(command) => command.is_api_command(),
            Self::Agent(command) => command.is_api_command(),
            Self::Terminal(command) => command.is_api_command(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigCommand {
    Check,
    Invalid,
}

impl ConfigCommand {
    fn parse(matches: &ArgMatches) -> Self {
        match matches.subcommand_name() {
            Some("check") => Self::Check,
            _ => Self::Invalid,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Invalid => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalCommand {
    Attach { terminal_id: String, takeover: bool },
    TitleSet { title: String },
    TitleClear,
    Invalid,
}

impl TerminalCommand {
    fn parse(matches: &ArgMatches) -> Self {
        match matches.subcommand() {
            Some(("attach", command)) => Self::Attach {
                terminal_id: matches::required(command, "terminal_id"),
                takeover: matches::flag(command, "takeover"),
            },
            Some(("title", title)) => match title.subcommand() {
                Some(("set", command)) => Self::TitleSet {
                    title: matches::required(command, "title"),
                },
                Some(("clear", _)) => Self::TitleClear,
                _ => Self::Invalid,
            },
            _ => Self::Invalid,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Attach { .. } => "attach",
            Self::TitleSet { .. } | Self::TitleClear => "title",
            Self::Invalid => "",
        }
    }

    fn is_api_command(&self) -> bool {
        matches!(self, Self::TitleSet { .. } | Self::TitleClear)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionCommand {
    List { json: bool },
    Stop { name: String, json: bool },
    Delete { name: String, json: bool },
    Invalid,
}

impl SessionCommand {
    fn parse(matches: &ArgMatches) -> Self {
        match matches.subcommand() {
            Some(("list", command)) => Self::List {
                json: matches::flag(command, "json"),
            },
            Some(("stop", command)) => Self::Stop {
                name: matches::required(command, "name"),
                json: matches::flag(command, "json"),
            },
            Some(("delete", command)) => Self::Delete {
                name: matches::required(command, "name"),
                json: matches::flag(command, "json"),
            },
            // `session attach` is a launch mode and is handled above.
            _ => Self::Invalid,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Stop { .. } => "stop",
            Self::Delete { .. } => "delete",
            Self::Invalid => "",
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
    remote: Option<String>,
    remote_keybindings: Option<String>,
    help: bool,
    version: bool,
    default_config: bool,
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
                Some(("server", matches)) if matches.subcommand().is_none() => {
                    Launch::HeadlessServer
                }
                Some(("client", _)) => Launch::Client,
                Some(("remote-api-bridge", matches)) => Launch::ApiBridge {
                    check: matches::flag(matches, "check"),
                },
                Some(("remote-client-bridge", matches)) => Launch::ClientBridge {
                    idle_timeout_v1: matches::flag(matches, "idle-timeout-v1"),
                },
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
                    None => {
                        shepr_platform::begin_cli_output();
                        eprintln!(
                            "error: command '{name}' has no typed parser; run with --help for usage"
                        );
                        return Err(2);
                    }
                },
            };
            Ok(Invocation {
                launch,
                session: matches::string(&matches, "session"),
                machine: matches::string(&matches, "machine"),
                remote: matches::string(&matches, "remote"),
                remote_keybindings: matches::string(&matches, "remote-keybindings"),
                help: matches::flag(&matches, "help"),
                version: matches::flag(&matches, "version"),
                default_config: matches::flag(&matches, "default-config"),
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

    pub(crate) fn remote(&self) -> Option<String> {
        self.remote.clone()
    }

    pub(crate) fn remote_keybindings(&self) -> Option<String> {
        self.remote_keybindings.clone()
    }

    pub(crate) fn help_requested(&self) -> bool {
        self.help
    }

    pub(crate) fn version_requested(&self) -> bool {
        self.version
    }

    pub(crate) fn default_config_requested(&self) -> bool {
        self.default_config
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
    pub(crate) fn has_subcommand(&self) -> bool {
        !matches!(
            &self.launch,
            Launch::Tui {
                attached_session: None
            }
        )
    }
}

pub(crate) fn print_help(requested_session: Option<crate::config::SessionId>) {
    shepr_platform::begin_cli_output();
    let help = spec::command().render_help().to_string();
    print!("{help}");
    if !help.ends_with("\n\n") {
        println!();
    }
    match crate::config::AppPaths::resolve_with_session(requested_session) {
        Ok(paths) => {
            println!("Config: {}", paths.config_file().display());
            println!(
                "Logs:   {}",
                crate::logging::help_log_paths_summary(&crate::session::data_dir(&paths))
            );
        }
        Err(errors) => {
            println!("Config: unavailable ({})", errors.join("; "));
            println!("Logs:   unavailable ({})", errors.join("; "));
        }
    }
    println!("Env:    SHEPR_CONFIG_PATH overrides config file path");
}

pub(super) fn print_read_response(response: &serde_json::Value) -> CliResult<i32> {
    if response.get("error").is_some() {
        eprintln!("{response}");
        return Ok(1);
    }
    if let Some(text) = response["result"]["read"]["text"].as_str() {
        print!("{text}");
    }
    Ok(0)
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
    requested_session: Option<crate::config::SessionId>,
) -> CliResult<i32> {
    if matches!(command, CliCommand::Config(ConfigCommand::Check)) {
        return Ok(config_check());
    }
    let paths = resolve_app_paths(requested_session)?;
    let context = target::CliContext::local(paths);
    dispatch_with_config(command, None, &context)
}

fn dispatch_with_config(
    command: &CliCommand,
    config: Option<crate::config::ValidatedConfig>,
    context: &target::CliContext,
) -> CliResult<i32> {
    match command {
        CliCommand::Status(command) => status::run_status_command(*command, context),
        CliCommand::Config(ConfigCommand::Check) => Ok(config_check()),
        CliCommand::Config(ConfigCommand::Invalid) => Ok(missing_subcommand()),
        CliCommand::Machine(command) => machine::run_machine_command(command.clone(), context),
        CliCommand::Server(command) => server::run_server_command(*command, context),
        CliCommand::Workspace(command) => {
            workspace::run_workspace_command(command.clone(), context)
        }
        CliCommand::Tab(command) => tab::run_tab_command(command.clone(), context),
        CliCommand::Agent(command) => agent::run_agent_command(command.clone(), config, context),
        CliCommand::Pane(command) => pane::run_pane_command(command.clone(), context),
        CliCommand::Terminal(command) => run_terminal_command(command.clone(), config, context),
        CliCommand::Session(command) => run_session_command(command.clone(), context),
        CliCommand::Integration(command) => {
            integration::run_integration_command(command.clone(), context)
        }
    }
}

fn resolve_app_paths(
    requested_session: Option<crate::config::SessionId>,
) -> CliResult<crate::config::AppPaths> {
    crate::config::AppPaths::resolve_with_session(requested_session).map_err(|diagnostics| {
        CliError::Io(std::io::Error::other(format!(
            "application paths could not be resolved:\n  {}",
            diagnostics.join("\n  ")
        )))
    })
}

fn resolve_machine_app_paths() -> CliResult<crate::config::AppPaths> {
    crate::config::AppPaths::resolve_for_machine().map_err(|diagnostics| {
        CliError::Io(std::io::Error::other(format!(
            "application paths could not be resolved:\n  {}",
            diagnostics.join("\n  ")
        )))
    })
}

/// The spec makes every command group require a subcommand, so this only
/// runs if a handler and the spec disagree about the subcommand names.
pub(super) fn missing_subcommand() -> i32 {
    let error = CliError::Usage("missing or unknown subcommand".into());
    error.print();
    error.exit_code()
}

/// A usage error found after clap accepted the arguments (a combination the
/// spec cannot express). Same exit code as clap's own usage errors.
pub(super) fn usage_error(message: &str) -> i32 {
    let error = CliError::Usage(message.into());
    error.print();
    error.exit_code()
}

fn config_check() -> i32 {
    // Path problems are reported like any other config issue instead of
    // aborting the check.
    let (diagnostics, provenance) = match crate::config::AppPaths::resolve() {
        Ok(paths) => {
            let loaded = crate::config::Config::load_for_check(&paths);
            let mut sources = loaded
                .provenance
                .values()
                .iter()
                .map(|origin| format!("{} = {} <- {}", origin.key, origin.value, origin.source))
                .collect::<Vec<_>>();
            let path_sources = paths.provenance();
            let home = paths.home_dir().map_or_else(
                || "unavailable".to_owned(),
                |path| path.display().to_string(),
            );
            let current = paths.current_dir().map_or_else(
                || "unavailable".to_owned(),
                |path| path.display().to_string(),
            );
            sources.extend([
                format!(
                    "paths.config_dir={} <- {}",
                    paths.config_dir().display(),
                    path_sources.config_dir
                ),
                format!(
                    "paths.state_dir={} <- {}",
                    paths.state_dir().display(),
                    path_sources.state_dir
                ),
                format!(
                    "paths.config_file={} <- {}",
                    paths.config_file().display(),
                    path_sources.config_file
                ),
                format!("paths.home_dir={home} <- {}", path_sources.home_dir),
                format!(
                    "paths.current_dir={current} <- {}",
                    path_sources.current_dir
                ),
                format!(
                    "paths.session_id={} <- {}",
                    paths.session_id().display_name(),
                    path_sources.session_id
                ),
                format!(
                    "paths.api_socket={} <- {}",
                    paths.server_address().api_socket().display(),
                    path_sources.api_socket
                ),
                format!(
                    "paths.client_socket={} <- {}",
                    paths.server_address().client_socket().display(),
                    path_sources.client_socket
                ),
            ]);
            (loaded.diagnostics, sources)
        }
        Err(diagnostics) => (
            diagnostics
                .into_iter()
                .map(crate::config::ConfigDiagnostic::Path)
                .collect(),
            Vec::new(),
        ),
    };
    if diagnostics.is_empty() {
        println!("config: ok");
    } else {
        println!("config: issues found");
        for diagnostic in &diagnostics {
            println!("{diagnostic}");
        }
    }
    println!("resolved sources:");
    for source in provenance {
        println!("  {source}");
    }

    i32::from(!diagnostics.is_empty())
}

fn load_validated_config(
    paths: &crate::config::AppPaths,
) -> CliResult<crate::config::ValidatedConfig> {
    crate::config::Config::load_validated(paths).map_err(|diagnostics| {
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

fn run_terminal_command(
    command: TerminalCommand,
    config: Option<crate::config::ValidatedConfig>,
    context: &target::CliContext,
) -> CliResult<i32> {
    match command {
        TerminalCommand::Attach {
            terminal_id,
            takeover,
        } => {
            let config = match config {
                Some(config) => config,
                None => load_validated_config(context)?,
            };
            crate::client::run_terminal_attach(&config, context, terminal_id, takeover)?;
            Ok(0)
        }
        TerminalCommand::TitleSet { title } => print_response(&send_request(
            context,
            &Request {
                id: "cli:terminal:title:set".into(),
                method: Method::ClientWindowTitleSet(ClientWindowTitleSetParams { title }),
            },
        )?),
        TerminalCommand::TitleClear => print_response(&send_request(
            context,
            &Request {
                id: "cli:terminal:title:clear".into(),
                method: Method::ClientWindowTitleClear(EmptyParams::default()),
            },
        )?),
        TerminalCommand::Invalid => Ok(missing_subcommand()),
    }
}

fn run_session_command(command: SessionCommand, paths: &target::CliContext) -> CliResult<i32> {
    match command {
        SessionCommand::List { json } => session_list(paths, json),
        SessionCommand::Stop { name, json } => session_stop(&name, json, paths),
        SessionCommand::Delete { name, json } => session_delete(&name, json, paths),
        SessionCommand::Invalid => Ok(missing_subcommand()),
    }
}

fn session_list(paths: &crate::config::AppPaths, json: bool) -> CliResult<i32> {
    let sessions = crate::session::list_sessions(paths)?;
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

/// Deliberately skips the protocol check that `send_request` does: the
/// protocol-mismatch error tells the user to run `session stop` / `server
/// stop`, so stopping must keep working against a server from another build.
/// `crate::session` sends a bare `server.stop` JSON line for that reason.
fn session_stop(name: &str, json: bool, paths: &crate::config::AppPaths) -> CliResult<i32> {
    let target = crate::session::parse_target_name(name)
        .map_err(|message| CliError::Session(SessionCliError::InvalidName(message)))?;
    match crate::session::stop_session(paths, &target) {
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

fn session_delete(name: &str, json: bool, paths: &crate::config::AppPaths) -> CliResult<i32> {
    let target = crate::session::parse_target_name(name)
        .map_err(|message| CliError::Session(SessionCliError::InvalidName(message)))?;
    match crate::session::delete_session(paths, &target) {
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

pub(super) fn print_response(response: &serde_json::Value) -> CliResult<i32> {
    if response.get("error").is_some() {
        eprintln!(
            "{}",
            serde_json::to_string(response).map_err(std::io::Error::other)?
        );
        return Ok(1);
    }

    println!(
        "{}",
        serde_json::to_string(response).map_err(std::io::Error::other)?
    );
    Ok(0)
}

fn send_ok_request(context: &target::CliContext, method: Method) -> CliResult<i32> {
    let response = send_request(
        context,
        &Request {
            id: "cli:request".into(),
            method,
        },
    )?;

    if response.get("error").is_some() {
        eprintln!(
            "{}",
            serde_json::to_string(&response).map_err(std::io::Error::other)?
        );
        return Ok(1);
    }

    Ok(0)
}

fn send_request(context: &target::CliContext, request: &Request) -> CliResult<serde_json::Value> {
    let client = target::api_client(context)?;
    ensure_server_protocol_compatible(context, &client, &request.id)?;
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

fn ensure_server_protocol_compatible(
    context: &target::CliContext,
    client: &ApiClient,
    request_id: &str,
) -> CliResult<()> {
    // Checked once per target: a polling command must not pay a status round
    // trip (up to 15 s under `--machine`) before every request.
    if context.protocol_checked() {
        return Ok(());
    }
    let status = target::server_status(context, client)
        .map_err(|err| map_server_not_running_or_io(context, err, request_id, client))?;
    let server_protocol = match crate::protocol::Compatibility::of(status.protocol) {
        crate::protocol::Compatibility::Compatible => {
            context.mark_protocol_checked();
            return Ok(());
        }
        crate::protocol::Compatibility::DifferentBuild(protocol) => protocol,
        crate::protocol::Compatibility::Unknown => {
            return Err(
                std::io::Error::other("server ping did not include a protocol version").into(),
            );
        }
    };
    let response = protocol_guard::mismatch_response(
        request_id,
        server_protocol,
        &target::restart_guidance(context),
    );

    Err(protocol_guard::cli_error(response))
}

/// Whether the local API socket is definitely absent or stale. Other probe
/// failures remain transport errors because they do not establish liveness.
pub(super) fn server_not_running_error(socket_path: &std::path::Path) -> CliResult<bool> {
    match crate::ipc::probe(socket_path) {
        crate::ipc::Liveness::Absent | crate::ipc::Liveness::Stale => Ok(true),
        crate::ipc::Liveness::Live => Ok(false),
        crate::ipc::Liveness::Unreachable(error) => Err(error.into()),
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

fn print_session_table(sessions: &[crate::session::SessionInfo]) {
    println!("{:<20} {:<8} {:<48} socket", "name", "status", "directory");
    for session in sessions {
        println!(
            "{:<20} {:<8} {:<48} {}",
            session.name,
            if session.running {
                "running"
            } else {
                "stopped"
            },
            session.session_dir.display(),
            session.socket_path.display()
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

impl<'a> From<&'a crate::session::SessionInfo> for SessionInfoJson<'a> {
    fn from(info: &'a crate::session::SessionInfo) -> Self {
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
        let invocation = parse(&["--session", "work", "workspace", "list"]);
        assert_eq!(invocation.session().as_deref(), Some("work"));
        assert!(matches!(
            invocation.launch,
            Launch::Cli(command) if matches!(*command, CliCommand::Workspace(_))
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
        // Text for `pane run` passes through untouched, including words that
        // look like launch options and a second `--`.
        let invocation = parse(&["pane", "run", "p1", "foo", "--session", "work"]);
        assert_eq!(invocation.session(), None);
        let run = command_matches(&["pane", "run", "p1", "foo", "--remote", "x", "--", "y"]);
        assert_eq!(
            super::matches::words(&run, "command"),
            "foo --remote x -- y"
        );
        let run = command_matches(&["pane", "run", "p1", "--", "--session", "work"]);
        assert_eq!(super::matches::words(&run, "command"), "--session work");

        // Arguments after `--` for `agent start` belong to the agent.
        let invocation = parse(&[
            "agent",
            "start",
            "repro",
            "--kind",
            "claude",
            "--pane",
            "p1",
            "--",
            "/bin/echo",
            "--session",
            "child-session",
            "--session=child-session",
        ]);
        assert_eq!(invocation.session(), None);

        // Elsewhere a trailing launch option is a usage error, not a silent
        // retarget of the command.
        for args in [
            &["server", "stop", "--session=api"][..],
            &["workspace", "list", "--session", "work"],
            &["pane", "list", "--remote", "host"],
            &["agent", "list", "--machine", "mac"],
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
        let CliCommand::Session(SessionCommand::Stop { name, json }) = *command else {
            panic!("session stop should be a typed CLI command");
        };
        assert_eq!(name, "-work");
        assert!(json);
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
    fn terminal_and_agent_attach_reject_invalid_config_before_connecting() {
        let _env = crate::test_support::IsolatedEnv::new();
        let scratch = crate::test_support::ScratchDir::new("cli-invalid-config");
        let config_path = scratch.join("config.toml");
        std::fs::write(&config_path, "[").expect("write invalid config");
        _env.set(crate::config::CONFIG_PATH_ENV_VAR, &config_path);

        for args in [
            &["terminal", "attach", "terminal-1"][..],
            &["agent", "attach", "agent-1"],
        ] {
            let invocation = parse(args);
            let Launch::Cli(command) = invocation.launch else {
                panic!("{args:?} is not a CLI command");
            };
            let error = match super::run(command.as_ref(), None) {
                Err(error) => error,
                Ok(code) => {
                    panic!("{args:?} unexpectedly returned exit code {code}")
                }
            };
            assert!(
                error.to_string().contains("configuration error"),
                "{args:?} should fail on invalid config before connecting: {error}"
            );
        }
    }

    #[test]
    fn session_name_accepts_option_terminator() {
        for name in ["-h", "--json"] {
            let stop = command_matches(&["session", "stop", "--", name]);
            assert_eq!(super::matches::required(&stop, "name"), name);
            assert!(!super::matches::flag(&stop, "json"));
        }
    }

    #[test]
    fn equals_form_works_for_every_value_option() {
        let split = command_matches(&[
            "pane",
            "split",
            "--direction=right",
            "--cwd=/var/tmp",
            "--ratio=0.5",
        ]);
        assert_eq!(
            super::matches::string(&split, "cwd").as_deref(),
            Some("/var/tmp")
        );
        assert_eq!(super::matches::value::<f32>(&split, "ratio"), Some(0.5));

        let create = command_matches(&["workspace", "create", "--label=dev", "--env=A=b"]);
        assert_eq!(
            super::matches::string(&create, "label").as_deref(),
            Some("dev")
        );
        assert_eq!(
            super::matches::values::<(String, String)>(&create, "env"),
            vec![("A".to_string(), "b".to_string())]
        );
    }

    #[test]
    fn hidden_launch_modes_keep_typed_options() {
        assert!(matches!(
            parse(&[
                "--session",
                "work",
                "remote-client-bridge",
                "--idle-timeout-v1"
            ])
            .launch,
            Launch::ClientBridge {
                idle_timeout_v1: true
            }
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
            &["pane"],
            &["config", "reset-keys"],
        ] {
            assert_eq!(parse_error(args).exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn parse_env_assignment_accepts_empty_values() {
        assert_eq!(
            super::parse_env_assignment("SHEPR_ROLE=").expect("test precondition"),
            ("SHEPR_ROLE".to_string(), String::new())
        );
    }

    #[test]
    fn parse_env_assignment_requires_key_value_separator() {
        assert_eq!(
            super::parse_env_assignment("SHEPR_ROLE").expect_err("test precondition"),
            "env must use KEY=VALUE"
        );
    }

    #[test]
    fn maps_dead_server_connect_failure_to_friendly_error() {
        use crate::api::client::{ApiClient, ApiClientError};

        let scratch = crate::test_support::ScratchDir::new("cli-socket-error");
        let paths =
            super::target::CliContext::test_local(crate::config::AppPaths::test_at(scratch.path()));
        let client = ApiClient::local(&paths);
        let socket = client.socket_path().display().to_string();

        // Classification carries the response to the final CLI printer.
        let mapped = super::map_server_not_running_or_io(
            &paths,
            ApiClientError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
            "cli:workspace:create",
            &client,
        );

        let response = super::server_not_running::reported_response(&mapped)
            .expect("dead-server connect failure should carry a server_not_running response");
        assert_eq!(response.id, "cli:workspace:create");
        assert_eq!(response.error.code, "server_not_running");
        assert!(response.error.message.contains(&socket));

        // The typed error preserves the response without string matching.
        assert!(super::server_not_running::was_reported(&mapped));
    }

    #[test]
    fn session_json_formats_paths_at_the_cli_edge() {
        let info = crate::session::SessionInfo {
            name: "work".into(),
            default: false,
            running: true,
            socket_path: "/tmp/work/shepr.sock".into(),
            session_dir: "/tmp/work".into(),
        };
        let value = serde_json::to_value(super::SessionInfoJson::from(&info))
            .expect("session CLI JSON serializes");
        assert_eq!(value["socket_path"], "/tmp/work/shepr.sock");
        assert_eq!(value["session_dir"], "/tmp/work");
    }

    #[test]
    fn classifier_ignores_unrelated_io_kinds() {
        use crate::api::client::{ApiClient, ApiClientError};

        let scratch = crate::test_support::ScratchDir::new("cli-socket-classifier");
        let paths =
            super::target::CliContext::test_local(crate::config::AppPaths::test_at(scratch.path()));
        std::fs::create_dir_all(paths.runtime_dir()).expect("create test runtime directory");
        let client = ApiClient::local(&paths);
        let _listener = crate::ipc::bind_local_listener(&client.socket_path())
            .expect("bind test server socket");
        let mapped = super::map_server_not_running_or_io(
            &paths,
            ApiClientError::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)),
            "cli:workspace:create",
            &client,
        );
        assert!(!super::server_not_running::was_reported(&mapped));
    }

    #[test]
    fn metadata_tokens_apply_in_argument_order() {
        let report = command_matches(&[
            "workspace",
            "report-metadata",
            "w1",
            "--source",
            "s",
            "--token",
            "a=1",
            "--clear-token",
            "a",
            "--clear-token",
            "b",
            "--token",
            "b=2",
        ]);
        let tokens = super::matches::metadata_tokens(&report);
        assert_eq!(tokens.get("a"), Some(&None));
        assert_eq!(tokens.get("b"), Some(&Some("2".to_string())));
    }
}
