use clap::ArgMatches;

use crate::api::client::{ApiClient, ApiClientError};
use crate::api::schema::{ClientWindowTitleSetParams, EmptyParams, Method, Request};

macro_rules! print {
    ($($arg:tt)*) => {{
        crate::platform::begin_cli_output();
        std::print!($($arg)*);
    }};
}

macro_rules! println {
    ($($arg:tt)*) => {{
        crate::platform::begin_cli_output();
        std::println!($($arg)*);
    }};
}

mod agent;
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

/// A top-level command after clap has parsed argv once. The variants make
/// launch modes and CLI command groups explicit while retaining each group's
/// parsed values for the existing handlers.
pub(crate) enum Launch {
    Tui { attached_session: Option<String> },
    HeadlessServer,
    Client,
    ApiBridge { check: bool },
    ClientBridge { idle_timeout_v1: bool },
    Cli(CliCommand),
}

pub(crate) enum CliCommand {
    Status(ArgMatches),
    Config(ArgMatches),
    Machine(ArgMatches),
    Server(ArgMatches),
    Workspace(ArgMatches),
    Tab(ArgMatches),
    Agent(ArgMatches),
    Pane(ArgMatches),
    Terminal(ArgMatches),
    Session(ArgMatches),
    Integration(ArgMatches),
    Other { name: String, matches: ArgMatches },
}

impl CliCommand {
    fn from_matches(name: &str, matches: &ArgMatches) -> Self {
        let matches = matches.clone();
        match name {
            "status" => Self::Status(matches),
            "config" => Self::Config(matches),
            "machine" => Self::Machine(matches),
            "server" => Self::Server(matches),
            "workspace" => Self::Workspace(matches),
            "tab" => Self::Tab(matches),
            "agent" => Self::Agent(matches),
            "pane" => Self::Pane(matches),
            "terminal" => Self::Terminal(matches),
            "session" => Self::Session(matches),
            "integration" => Self::Integration(matches),
            _ => Self::Other {
                name: name.to_owned(),
                matches,
            },
        }
    }

    fn parts(&self) -> (&str, &ArgMatches) {
        match self {
            Self::Status(matches) => ("status", matches),
            Self::Config(matches) => ("config", matches),
            Self::Machine(matches) => ("machine", matches),
            Self::Server(matches) => ("server", matches),
            Self::Workspace(matches) => ("workspace", matches),
            Self::Tab(matches) => ("tab", matches),
            Self::Agent(matches) => ("agent", matches),
            Self::Pane(matches) => ("pane", matches),
            Self::Terminal(matches) => ("terminal", matches),
            Self::Session(matches) => ("session", matches),
            Self::Integration(matches) => ("integration", matches),
            Self::Other { name, matches } => (name, matches),
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
                Some((name, matches)) => Launch::Cli(CliCommand::from_matches(name, matches)),
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
            crate::platform::begin_cli_output();
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

pub(crate) fn print_help(requested_session: Option<crate::session::SessionId>) {
    crate::platform::begin_cli_output();
    let help = spec::command().render_help().to_string();
    print!("{help}");
    if !help.ends_with("\n\n") {
        println!();
    }
    match crate::config::AppPaths::resolve_with_session(requested_session) {
        Ok(paths) => {
            println!("Config: {}", paths.config_file().display());
            println!("Logs:   {}", crate::logging::help_log_paths_summary(&paths));
        }
        Err(errors) => {
            println!("Config: unavailable ({})", errors.join("; "));
            println!("Logs:   unavailable ({})", errors.join("; "));
        }
    }
    println!("Env:    SHEPR_CONFIG_PATH overrides config file path");
}

pub(super) fn print_read_response(response: &serde_json::Value) -> std::io::Result<i32> {
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
pub(crate) fn run_on_machine(command: Option<&CliCommand>, selector: &str) -> std::io::Result<i32> {
    let paths = resolve_machine_app_paths()?;
    target::run_on_machine(selector, command, &paths)
}

/// Runs one parsed CLI command. Launch modes are handled by `main` directly.
pub(crate) fn run(
    command: &CliCommand,
    requested_session: Option<crate::session::SessionId>,
) -> std::io::Result<i32> {
    if let CliCommand::Config(matches) = command
        && matches
            .subcommand()
            .is_some_and(|(name, _)| name == "check")
    {
        return Ok(run_config_command(matches));
    }
    let paths = resolve_app_paths(requested_session)?;
    let context = target::CliContext::local(paths);
    dispatch_with_config(command, None, &context)
}

fn dispatch_with_config(
    command: &CliCommand,
    config: Option<crate::config::Config>,
    context: &target::CliContext,
) -> std::io::Result<i32> {
    let (_, matches) = command.parts();
    match command {
        CliCommand::Status(_) => status::run_status_command(matches, context),
        CliCommand::Config(_) => Ok(run_config_command(matches)),
        CliCommand::Machine(_) => machine::run_machine_command(matches, context),
        CliCommand::Server(_) => server::run_server_command(matches, context),
        CliCommand::Workspace(_) => workspace::run_workspace_command(matches, context),
        CliCommand::Tab(_) => tab::run_tab_command(matches, context),
        CliCommand::Agent(_) => agent::run_agent_command(matches, config, context),
        CliCommand::Pane(_) => pane::run_pane_command(matches, context),
        CliCommand::Terminal(_) => run_terminal_command(matches, config, context),
        CliCommand::Session(_) => run_session_command(matches, context),
        CliCommand::Integration(_) => integration::run_integration_command(matches, context),
        CliCommand::Other { .. } => Ok(missing_subcommand()),
    }
}

fn resolve_app_paths(
    requested_session: Option<crate::session::SessionId>,
) -> std::io::Result<crate::config::AppPaths> {
    crate::config::AppPaths::resolve_with_session(requested_session).map_err(|diagnostics| {
        std::io::Error::other(format!(
            "application paths could not be resolved:\n  {}",
            diagnostics.join("\n  ")
        ))
    })
}

fn resolve_machine_app_paths() -> std::io::Result<crate::config::AppPaths> {
    crate::config::AppPaths::resolve_for_machine().map_err(|diagnostics| {
        std::io::Error::other(format!(
            "application paths could not be resolved:\n  {}",
            diagnostics.join("\n  ")
        ))
    })
}

/// The spec makes every command group require a subcommand, so this only
/// runs if a handler and the spec disagree about the subcommand names.
pub(super) fn missing_subcommand() -> i32 {
    eprintln!("error: missing or unknown subcommand; run with --help for usage");
    2
}

/// A usage error found after clap accepted the arguments (a combination the
/// spec cannot express). Same exit code as clap's own usage errors.
pub(super) fn usage_error(message: &str) -> i32 {
    eprintln!("{message}");
    2
}

fn run_config_command(matches: &ArgMatches) -> i32 {
    match matches.subcommand() {
        Some(("check", _)) => config_check(),
        _ => missing_subcommand(),
    }
}

fn config_check() -> i32 {
    // Path problems are reported like any other config issue instead of
    // aborting the check.
    let diagnostics = match crate::config::AppPaths::resolve() {
        Ok(paths) => crate::config::Config::load_for_check(&paths).diagnostics,
        Err(diagnostics) => diagnostics,
    };
    if diagnostics.is_empty() {
        println!("config: ok");
    } else {
        println!("config: issues found");
        for diagnostic in &diagnostics {
            println!("{diagnostic}");
        }
    }

    i32::from(!diagnostics.is_empty())
}

fn load_validated_config(
    paths: &crate::config::AppPaths,
) -> std::io::Result<crate::config::Config> {
    crate::config::Config::load_validated(paths).map_err(|diagnostics| {
        std::io::Error::other(format!(
            "configuration error:\n  {}",
            diagnostics.join("\n  ")
        ))
    })
}

fn run_terminal_command(
    matches: &ArgMatches,
    config: Option<crate::config::Config>,
    context: &target::CliContext,
) -> std::io::Result<i32> {
    match matches.subcommand() {
        Some(("attach", matches)) => {
            let config = match config {
                Some(config) => config,
                None => load_validated_config(context)?,
            };
            crate::client::run_terminal_attach(
                &config,
                context,
                matches::required(matches, "terminal_id"),
                matches::flag(matches, "takeover"),
            )?;
            Ok(0)
        }
        Some(("title", matches)) => match matches.subcommand() {
            Some(("set", matches)) => print_response(&send_request(
                context,
                &Request {
                    id: "cli:terminal:title:set".into(),
                    method: Method::ClientWindowTitleSet(ClientWindowTitleSetParams {
                        title: matches::required(matches, "title"),
                    }),
                },
            )?),
            Some(("clear", _)) => print_response(&send_request(
                context,
                &Request {
                    id: "cli:terminal:title:clear".into(),
                    method: Method::ClientWindowTitleClear(EmptyParams::default()),
                },
            )?),
            _ => Ok(missing_subcommand()),
        },
        _ => Ok(missing_subcommand()),
    }
}

fn run_session_command(matches: &ArgMatches, paths: &target::CliContext) -> std::io::Result<i32> {
    match matches.subcommand() {
        Some(("list", matches)) => session_list(paths, matches::flag(matches, "json")),
        Some(("attach", _)) => Ok(missing_subcommand()),
        Some(("stop", matches)) => Ok(session_stop(
            &matches::required(matches, "name"),
            matches::flag(matches, "json"),
            paths,
        )),
        Some(("delete", matches)) => Ok(session_delete(
            &matches::required(matches, "name"),
            matches::flag(matches, "json"),
            paths,
        )),
        _ => Ok(missing_subcommand()),
    }
}

fn session_list(paths: &crate::config::AppPaths, json: bool) -> std::io::Result<i32> {
    let sessions = crate::session::list_sessions(paths)?;
    if json {
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
fn session_stop(name: &str, json: bool, paths: &crate::config::AppPaths) -> i32 {
    let target = match crate::session::parse_target_name(name) {
        Ok(target) => target,
        Err(message) => {
            print_session_error("invalid_session_name", &message);
            return 1;
        }
    };
    match crate::session::stop_session(paths, &target) {
        Ok(session) => {
            if json {
                print_json(&serde_json::json!({
                    "stopped": true,
                    "session": session,
                }));
            } else {
                println!("stopped session {}", session.name);
            }
            0
        }
        Err(message) => {
            print_session_error("session_stop_failed", &message);
            1
        }
    }
}

fn session_delete(name: &str, json: bool, paths: &crate::config::AppPaths) -> i32 {
    let target = match crate::session::parse_target_name(name) {
        Ok(target) => target,
        Err(message) => {
            print_session_error("invalid_session_name", &message);
            return 1;
        }
    };
    match crate::session::delete_session(paths, &target) {
        Ok(session) => {
            if json {
                print_json(&serde_json::json!({
                    "deleted": true,
                    "session": session,
                }));
            } else {
                println!("deleted session {}", session.name);
            }
            0
        }
        Err(message) => {
            print_session_error("session_delete_failed", &message);
            1
        }
    }
}

pub(super) fn print_response(response: &serde_json::Value) -> std::io::Result<i32> {
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

fn send_ok_request(context: &target::CliContext, method: Method) -> std::io::Result<i32> {
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

fn send_request(
    context: &target::CliContext,
    request: &Request,
) -> std::io::Result<serde_json::Value> {
    let client = target::api_client(context)?;
    ensure_server_protocol_compatible(context, &client, &request.id)?;
    client
        .request_value(request)
        .map_err(|err| map_server_not_running_or_io(context, err, &request.id, &client))
}

fn send_request_unchecked(
    context: &target::CliContext,
    request: &Request,
) -> std::io::Result<serde_json::Value> {
    let client = target::api_client(context)?;
    client
        .request_value(request)
        .map_err(|err| map_server_not_running_or_io(context, err, &request.id, &client))
}

fn ensure_server_protocol_compatible(
    context: &target::CliContext,
    client: &ApiClient,
    request_id: &str,
) -> std::io::Result<()> {
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
            return Err(std::io::Error::other(
                "server ping did not include a protocol version",
            ));
        }
    };
    let response = protocol_guard::mismatch_response(
        request_id,
        server_protocol,
        &target::restart_guidance(context),
    );

    eprintln!(
        "{}",
        serde_json::to_string(&response).map_err(std::io::Error::other)?
    );
    Err(protocol_guard::reported_error())
}

pub(crate) fn protocol_mismatch_was_reported(err: &std::io::Error) -> bool {
    protocol_guard::was_reported(err)
}

pub(crate) fn server_not_running_was_reported(err: &std::io::Error) -> bool {
    server_not_running::was_reported(err)
}

/// Returns the `ErrorResponse` carried by a `server_not_running` marker, if any,
/// so the edge that surfaces the error can print it exactly once.
pub(crate) fn server_not_running_reported_response(
    err: &std::io::Error,
) -> Option<&crate::api::schema::ErrorResponse> {
    server_not_running::reported_response(err)
}

/// True when an io::Error indicates nothing is listening on the API socket.
pub(super) fn server_not_running_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Maps an `ApiClientError` from a socket command into the io::Error that
/// bubbles up to `main`. A dead-server connect failure is reported as a
/// friendly `server_not_running` JSON error plus a recognizable marker; all
/// other errors fall through unchanged so existing handling is preserved.
fn map_server_not_running_or_io(
    context: &target::CliContext,
    err: ApiClientError,
    request_id: &str,
    client: &ApiClient,
) -> std::io::Error {
    if context.is_remote() {
        return target::remote_error(context, api_client_error_to_io(err));
    }
    match err {
        ApiClientError::Io(io_err) if server_not_running_error(&io_err) => {
            server_not_running::reported_error(server_not_running::response(
                request_id,
                &client.socket_path(),
                context,
            ))
        }
        err => api_client_error_to_io(err),
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
            session.session_dir,
            session.socket_path
        );
    }
}

fn print_session_error(code: &str, message: &str) {
    eprintln!(
        "{}",
        serde_json::json!({
            "error": {
                "code": code,
                "message": message,
            }
        })
    );
}

fn print_json(value: &serde_json::Value) {
    println!("{value}");
}

#[cfg(test)]
mod tests {
    use super::{CliCommand, Invocation, Launch};

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

    /// The sub-matches of `shepr <group> <command> ...`.
    pub(super) fn command_matches(args: &[&str]) -> clap::ArgMatches {
        let invocation = parse(args);
        let Launch::Cli(command) = &invocation.launch else {
            panic!("{args:?} is not a CLI command");
        };
        // A CLI launch carries the group's matches (`pane`, `tab`, ...).
        let (_, group) = command.parts();
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
            Launch::Cli(CliCommand::Workspace(_))
        ));

        let invocation = parse(&["--session=api", "server", "stop"]);
        assert_eq!(invocation.session().as_deref(), Some("api"));
        assert!(matches!(
            invocation.launch,
            Launch::Cli(CliCommand::Server(_))
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
            let error = match super::run(&command, None) {
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

        // The helper does NOT print; it returns a recognizable marker carrying
        // the ErrorResponse so the surfacing edge can print it exactly once.
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

        // The mapping is recognizable without string matching.
        assert!(super::server_not_running::was_reported(&mapped));
    }

    #[test]
    fn classifier_ignores_unrelated_io_kinds() {
        use crate::api::client::{ApiClient, ApiClientError};

        let scratch = crate::test_support::ScratchDir::new("cli-socket-classifier");
        let paths =
            super::target::CliContext::test_local(crate::config::AppPaths::test_at(scratch.path()));
        let client = ApiClient::local(&paths);
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
