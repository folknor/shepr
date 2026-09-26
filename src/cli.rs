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

pub enum CommandOutcome {
    Handled(i32),
    NotCli,
}

/// The command line, parsed once by the clap spec in `spec.rs`. Launch options
/// (`--session`, `--machine`, `--remote`, ...) are only recognised before the
/// subcommand; everything after it belongs to the subcommand.
pub(crate) struct Invocation {
    matches: ArgMatches,
}

/// Parses argv. On a usage error, or when `--help` for a subcommand was asked
/// for, clap's message has already been printed and the exit code is returned.
pub(crate) fn parse_invocation(args: &[String]) -> Result<Invocation, i32> {
    match spec::command().try_get_matches_from(args) {
        Ok(matches) => Ok(Invocation { matches }),
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
        matches::string(&self.matches, "session")
    }

    pub(crate) fn machine(&self) -> Option<String> {
        matches::string(&self.matches, "machine")
    }

    pub(crate) fn remote(&self) -> Option<String> {
        matches::string(&self.matches, "remote")
    }

    pub(crate) fn remote_keybindings(&self) -> Option<String> {
        matches::string(&self.matches, "remote-keybindings")
    }

    pub(crate) fn help_requested(&self) -> bool {
        matches::flag(&self.matches, "help")
    }

    pub(crate) fn version_requested(&self) -> bool {
        matches::flag(&self.matches, "version")
    }

    pub(crate) fn default_config_requested(&self) -> bool {
        matches::flag(&self.matches, "default-config")
    }

    pub(crate) fn command_name(&self) -> Option<&str> {
        self.matches.subcommand_name()
    }

    /// The name given to `session attach NAME`. That command is the default
    /// launch into the named session, the same as `--session NAME`.
    pub(crate) fn session_attach_name(&self) -> Option<String> {
        let ("session", session) = self.matches.subcommand()? else {
            return None;
        };
        let ("attach", attach) = session.subcommand()? else {
            return None;
        };
        Some(matches::required(attach, "name"))
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

    /// Arguments for the hidden bridge commands, in the shape their runners take.
    pub(crate) fn bridge_args(&self) -> Vec<String> {
        let Some((name, matches)) = self.matches.subcommand() else {
            return Vec::new();
        };
        let option = match name {
            "remote-client-bridge" => "idle-timeout-v1",
            "remote-api-bridge" => "check",
            _ => return Vec::new(),
        };
        if matches::flag(matches, option) {
            vec![format!("--{option}")]
        } else {
            Vec::new()
        }
    }
}

/// Whether `shepr <words...>` names a command (or command group) in the spec.
#[cfg(test)]
pub(crate) fn command_path_exists(words: &[&str]) -> bool {
    let mut command = spec::command();
    for word in words {
        let Some(next) = command.find_subcommand(word).cloned() else {
            return false;
        };
        command = next;
    }
    true
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
pub(crate) fn run_on_machine(
    invocation: &Invocation,
    selector: &str,
) -> std::io::Result<CommandOutcome> {
    target::run_on_machine(selector, invocation.matches.subcommand())
}

/// Runs the invocation's subcommand. `NotCli` means the invocation launches
/// something instead: the TUI (no subcommand, or `session attach`), the
/// headless server (bare `server`), or one of the hidden client/bridge modes.
pub(crate) fn run(invocation: &Invocation) -> std::io::Result<CommandOutcome> {
    match invocation.matches.subcommand() {
        Some((name, matches)) => dispatch(name, matches),
        None => Ok(CommandOutcome::NotCli),
    }
}

fn dispatch(name: &str, matches: &ArgMatches) -> std::io::Result<CommandOutcome> {
    let exit_code = match name {
        "server" => {
            let Some(exit_code) = server::run_server_command(matches)? else {
                return Ok(CommandOutcome::NotCli);
            };
            exit_code
        }
        "status" => status::run_status_command(matches)?,
        "config" => run_config_command(matches),
        "machine" => machine::run_machine_command(matches)?,
        "workspace" => workspace::run_workspace_command(matches)?,
        "tab" => tab::run_tab_command(matches)?,
        "agent" => agent::run_agent_command(matches)?,
        "terminal" => run_terminal_command(matches)?,
        "pane" => pane::run_pane_command(matches)?,
        "integration" => run_integration_command(matches)?,
        "session" => {
            let Some(exit_code) = run_session_command(matches)? else {
                return Ok(CommandOutcome::NotCli);
            };
            exit_code
        }
        _ => return Ok(CommandOutcome::NotCli),
    };

    Ok(CommandOutcome::Handled(exit_code))
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
    let diagnostics = crate::config::Config::load().diagnostics;
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

fn run_integration_command(matches: &ArgMatches) -> std::io::Result<i32> {
    // The integration handlers take argv-shaped arguments; hand them the
    // values clap has already validated, in that shape.
    let args: Vec<String> = match matches.subcommand() {
        Some((action @ ("install" | "uninstall"), matches)) => {
            vec![action.to_string(), matches::required(matches, "target")]
        }
        Some(("status", matches)) => {
            let mut args = vec!["status".to_string()];
            if matches::flag(matches, "outdated-only") {
                args.push("--outdated-only".to_string());
            }
            args
        }
        _ => return Ok(missing_subcommand()),
    };
    integration::run_integration_command(&args)
}

fn run_terminal_command(matches: &ArgMatches) -> std::io::Result<i32> {
    match matches.subcommand() {
        Some(("attach", matches)) => {
            crate::client::run_terminal_attach(
                matches::required(matches, "terminal_id"),
                matches::flag(matches, "takeover"),
            )?;
            Ok(0)
        }
        Some(("title", matches)) => match matches.subcommand() {
            Some(("set", matches)) => print_response(&send_request(&Request {
                id: "cli:terminal:title:set".into(),
                method: Method::ClientWindowTitleSet(ClientWindowTitleSetParams {
                    title: matches::required(matches, "title"),
                }),
            })?),
            Some(("clear", _)) => print_response(&send_request(&Request {
                id: "cli:terminal:title:clear".into(),
                method: Method::ClientWindowTitleClear(EmptyParams::default()),
            })?),
            _ => Ok(missing_subcommand()),
        },
        _ => Ok(missing_subcommand()),
    }
}

/// `None` for `session attach`, which is a TUI launch rather than a command.
fn run_session_command(matches: &ArgMatches) -> std::io::Result<Option<i32>> {
    match matches.subcommand() {
        Some(("list", matches)) => session_list(matches::flag(matches, "json")).map(Some),
        Some(("attach", _)) => Ok(None),
        Some(("stop", matches)) => Ok(Some(session_stop(
            &matches::required(matches, "name"),
            matches::flag(matches, "json"),
        ))),
        Some(("delete", matches)) => Ok(Some(session_delete(
            &matches::required(matches, "name"),
            matches::flag(matches, "json"),
        ))),
        _ => Ok(Some(missing_subcommand())),
    }
}

fn session_list(json: bool) -> std::io::Result<i32> {
    let sessions = crate::session::list_sessions()?;
    if json {
        print_json(&serde_json::json!({
            "sessions": sessions,
        }));
    } else {
        print_session_table(&sessions);
    }
    Ok(0)
}

fn session_stop(name: &str, json: bool) -> i32 {
    let target = match crate::session::parse_target_name(name) {
        Ok(target) => target,
        Err(message) => {
            print_session_error("invalid_session_name", &message);
            return 1;
        }
    };
    match crate::session::stop_session(target.as_deref()) {
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

fn session_delete(name: &str, json: bool) -> i32 {
    match crate::session::delete_session(name) {
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

pub(super) fn send_ok_request(method: Method) -> std::io::Result<i32> {
    let response = send_request(&Request {
        id: "cli:request".into(),
        method,
    })?;

    if response.get("error").is_some() {
        eprintln!(
            "{}",
            serde_json::to_string(&response).map_err(std::io::Error::other)?
        );
        return Ok(1);
    }

    Ok(0)
}

pub(super) fn send_request(request: &Request) -> std::io::Result<serde_json::Value> {
    let client = target::api_client()?;
    ensure_server_protocol_compatible(&client, &request.id)?;
    client
        .request_value(request)
        .map_err(|err| map_server_not_running_or_io(err, &request.id, &client))
}

pub(super) fn send_request_unchecked(request: &Request) -> std::io::Result<serde_json::Value> {
    let client = target::api_client()?;
    client
        .request_value(request)
        .map_err(|err| map_server_not_running_or_io(err, &request.id, &client))
}

fn ensure_server_protocol_compatible(client: &ApiClient, request_id: &str) -> std::io::Result<()> {
    let status = target::server_status(client)
        .map_err(|err| map_server_not_running_or_io(err, request_id, client))?;
    let server_protocol = status
        .protocol
        .ok_or_else(|| std::io::Error::other("server ping did not include a protocol version"))?;
    let Some(response) =
        protocol_guard::mismatch_response(request_id, server_protocol, &target::restart_guidance())
    else {
        return Ok(());
    };

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
    err: ApiClientError,
    request_id: &str,
    client: &ApiClient,
) -> std::io::Error {
    if target::is_remote() {
        return target::remote_error(api_client_error_to_io(err));
    }
    match err {
        ApiClientError::Io(io_err) if server_not_running_error(&io_err) => {
            server_not_running::reported_error(server_not_running::response(
                request_id,
                &client.socket_path(),
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
    use super::{CommandOutcome, Invocation};

    pub(super) fn parse(args: &[&str]) -> Invocation {
        let mut argv = vec!["shepr".to_string()];
        argv.extend(args.iter().map(ToString::to_string));
        match super::spec::command().try_get_matches_from(&argv) {
            Ok(matches) => Invocation { matches },
            Err(error) => panic!("{args:?} should parse: {error}"),
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
        let Some((_, group)) = invocation.matches.subcommand() else {
            panic!("{args:?} has no command");
        };
        let Some((_, command)) = group.subcommand() else {
            panic!("{args:?} has no subcommand");
        };
        command.clone()
    }

    #[test]
    fn launch_options_are_read_before_the_subcommand() {
        let invocation = parse(&["--session", "work", "workspace", "list"]);
        assert_eq!(invocation.session().as_deref(), Some("work"));
        assert_eq!(invocation.command_name(), Some("workspace"));

        let invocation = parse(&["--session=api", "server", "stop"]);
        assert_eq!(invocation.session().as_deref(), Some("api"));
        assert_eq!(invocation.command_name(), Some("server"));
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
        assert!(matches!(
            super::run(&invocation).expect("test precondition"),
            CommandOutcome::NotCli
        ));

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
    fn remote_bridge_options_round_trip() {
        assert_eq!(
            parse(&[
                "--session",
                "work",
                "remote-client-bridge",
                "--idle-timeout-v1"
            ])
            .bridge_args(),
            vec!["--idle-timeout-v1"]
        );
        assert_eq!(
            parse(&["remote-api-bridge", "--check"]).bridge_args(),
            vec!["--check"]
        );
        assert!(parse(&["remote-api-bridge"]).bridge_args().is_empty());
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

        let client = ApiClient::local();
        let socket = client.socket_path().display().to_string();

        // The helper does NOT print; it returns a recognizable marker carrying
        // the ErrorResponse so the surfacing edge can print it exactly once.
        let mapped = super::map_server_not_running_or_io(
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

        let client = ApiClient::local();
        let mapped = super::map_server_not_running_or_io(
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
