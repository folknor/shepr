use serde::Serialize;

use shepr_api::schema::{ClientStatusJson, ServerStatusJson};
use shepr_api::{RuntimeStatus, ServerPresence};
use shepr_remote::{COMMAND_CLIENT, COMMAND_SERVER, FLAG_JSON, option_name_from_flag};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Overview { json: bool },
    Server { json: bool },
}

pub(super) enum ParsedCommand {
    Local(Command),
    Client { json: bool },
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<ParsedCommand> {
    let root_json = super::matches::try_flag(matches, option_name_from_flag(FLAG_JSON)).ok()?;
    match matches.subcommand() {
        None => Some(ParsedCommand::Local(Command::Overview { json: root_json })),
        Some((COMMAND_SERVER, scope)) => {
            let command_json =
                super::matches::try_flag(scope, option_name_from_flag(FLAG_JSON)).ok()?;
            Some(ParsedCommand::Local(Command::Server {
                json: root_json || command_json,
            }))
        }
        Some((COMMAND_CLIENT, scope)) => {
            let command_json =
                super::matches::try_flag(scope, option_name_from_flag(FLAG_JSON)).ok()?;
            Some(ParsedCommand::Client {
                json: root_json || command_json,
            })
        }
        Some(_) => None,
    }
}

pub(super) fn run_status_command(
    command: Command,
    paths: &shepr_paths::AppPaths,
) -> super::CliResult<i32> {
    match command {
        Command::Overview { json } => print_full_status(paths, json),
        Command::Server { json } => print_server_status(paths, json),
    }
}

fn print_full_status(paths: &shepr_paths::AppPaths, json: bool) -> super::CliResult<i32> {
    let server = read_server_runtime_status(paths)?;

    if json {
        print_json(&FullStatusJson {
            local_client: client_status_json(),
            server: server_status_json(paths, &server),
        })?;
        return Ok(0);
    }

    println!("local client:");
    print_client_status_body(&client_status_json(), "  ");
    println!();
    println!("server:");
    let (compatible, restart_needed) = build_status_flags(&server);
    print_server_status_body(paths, &server, "  ", compatible);
    println!();
    println!("update:");
    println!(
        "  restart_needed: {}",
        restart_needed_label(&server, restart_needed)
    );

    Ok(0)
}

fn print_server_status(paths: &shepr_paths::AppPaths, json: bool) -> super::CliResult<i32> {
    let server = read_server_runtime_status(paths)?;
    if json {
        print_json(&server_status_json(paths, &server))?;
        return Ok(0);
    }
    let (compatible, _) = build_status_flags(&server);
    print_server_status_body(paths, &server, "", compatible);
    Ok(0)
}

pub(super) fn print_client_status(json: bool) -> super::CliResult<()> {
    let status = client_status_json();
    if json {
        print_json(&status)?;
        return Ok(());
    }

    print_client_status_body(&status, "");
    Ok(())
}

/// The client's identity and the identity of the `shepr-server` installed
/// beside it, which a remote client's discovery checks against its own build.
fn print_client_status_body(status: &ClientStatusJson, indent: &str) {
    let identity = status.identity.as_ref();
    println!(
        "{indent}version: {}",
        option_label(identity.map(|identity| identity.version.as_str()))
    );
    println!(
        "{indent}build_id: {}",
        identity.map_or_else(
            || "unknown".into(),
            |identity| identity.build_id.to_string()
        )
    );
    if let Some(binary) = status.binary.as_deref() {
        println!("{indent}binary: {binary}");
    }
    let Some(server) = status.server.as_ref() else {
        return;
    };
    if let Some(binary) = server.binary.as_deref() {
        println!("{indent}server_binary: {binary}");
    }
    match &server.identity {
        Err(error) => println!("{indent}server_error: {error}"),
        Ok(identity) => {
            println!("{indent}server_version: {}", identity.version);
            println!("{indent}server_build_id: {}", identity.build_id);
        }
    }
}

fn print_server_status_body(
    paths: &shepr_paths::AppPaths,
    server: &ServerPresence,
    indent: &str,
    compatible: Option<bool>,
) {
    let label = match server {
        ServerPresence::Gone => "not running",
        ServerPresence::Starting(_) => "starting",
        ServerPresence::Running(_) => "running",
        ServerPresence::Stopping(_) => "stopping",
        ServerPresence::Unresponsive => "unresponsive",
    };
    println!("{indent}status: {label}");
    if let Some(status) = answered_status(server) {
        print_runtime_identity(status, indent);
    }
    if !matches!(server, ServerPresence::Gone) {
        println!(
            "{indent}build_compatible: {}",
            build_compatible_label(compatible)
        );
    }
    println!(
        "{indent}socket: {}",
        paths.server_address().socket().display()
    );
}

/// The identity a server answered with: present for starting, running and
/// stopping servers, absent for a gone or unresponsive one.
fn answered_status(server: &ServerPresence) -> Option<&RuntimeStatus> {
    match server {
        ServerPresence::Starting(status)
        | ServerPresence::Running(status)
        | ServerPresence::Stopping(status) => Some(status),
        ServerPresence::Gone | ServerPresence::Unresponsive => None,
    }
}

fn print_runtime_identity(status: &RuntimeStatus, indent: &str) {
    println!("{indent}version: {}", status.version);
    println!("{indent}build_id: {}", status.build_id);
    println!("{indent}boot_id: {}", status.boot_id);
}

fn read_server_runtime_status(paths: &shepr_paths::AppPaths) -> super::CliResult<ServerPresence> {
    Ok(shepr_api::read_server_presence_at(
        paths.server_address().socket(),
        crate::limits::STATUS_ANSWER_TIMEOUT,
    )?)
}

fn option_label(value: Option<&str>) -> &str {
    value.unwrap_or("unknown")
}

fn restart_needed_label(server: &ServerPresence, restart_needed: bool) -> &'static str {
    match server {
        ServerPresence::Unresponsive => "unknown",
        _ if restart_needed => "yes",
        _ => "no",
    }
}

#[derive(Serialize)]
struct FullStatusJson {
    local_client: ClientStatusJson,
    server: ServerStatusJson,
}

fn client_status_json() -> ClientStatusJson {
    ClientStatusJson {
        identity: Some(shepr_protocol::BuildVersion {
            version: shepr_protocol::PACKAGE_VERSION.to_owned(),
            build_id: shepr_protocol::BuildIdentity::for_this_build(),
        }),
        binary: Some(current_exe_label()),
        server: Some(shepr_remote::local_server::sibling_server_status()),
    }
}

fn server_status_json(paths: &shepr_paths::AppPaths, server: &ServerPresence) -> ServerStatusJson {
    use shepr_api::schema::{ServerIdentity, ServerStatus};
    let identity = |status: &RuntimeStatus| ServerIdentity {
        version: status.version.clone(),
        build_id: status.build_id,
        boot_id: status.boot_id.clone(),
    };
    let state = match server {
        ServerPresence::Gone => ServerStatus::Gone,
        ServerPresence::Starting(status) => ServerStatus::Starting(identity(status)),
        ServerPresence::Running(status) => ServerStatus::Running(identity(status)),
        ServerPresence::Stopping(status) => ServerStatus::Stopping(identity(status)),
        ServerPresence::Unresponsive => ServerStatus::Unresponsive,
    };
    ServerStatusJson {
        state,
        socket: paths.server_address().socket().display().to_string(),
    }
}

fn build_compatible_label(compatible: Option<bool>) -> &'static str {
    match compatible {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

/// A starting or running server of another build needs a restart; a stopping
/// one is already going away, and the successor is launched from this install.
fn build_status_flags(server: &ServerPresence) -> (Option<bool>, bool) {
    let compatible = answered_status(server).map(|status| status.build_id.is_this_build());
    let restart_needed = matches!(
        server,
        ServerPresence::Starting(_) | ServerPresence::Running(_)
    ) && compatible == Some(false);
    (compatible, restart_needed)
}

fn print_json(value: &impl Serialize) -> super::CliResult<()> {
    println!(
        "{}",
        serde_json::to_string(value).map_err(std::io::Error::other)?
    );
    Ok(())
}

fn current_exe_label() -> String {
    // Same resolution as every other place that names the binary, so a
    // replaced install reports its path, not "/…/shepr (deleted)".
    shepr_platform::launch_executable().map_or_else(
        |err| format!("unknown ({err})"),
        |path| path.display().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_api::schema::ServerPresenceJson;
    use shepr_test_fixtures::*;

    fn runtime_status(version: &str, build_id: &str) -> RuntimeStatus {
        RuntimeStatus {
            version: version.to_owned(),
            build_id: build_id.parse().expect("build identity"),
            boot_id: "4242-1700000000".parse().expect("boot identity"),
            lifecycle: shepr_api::RuntimeLifecycle::Running,
        }
    }

    fn running_server(version: &str, build_id: &str) -> ServerPresence {
        ServerPresence::Running(runtime_status(version, build_id))
    }

    fn test_paths() -> shepr_paths::AppPaths {
        shepr_paths::AppPaths::test_default()
    }

    #[test]
    fn server_status_json_reports_the_running_boot() {
        let server = running_server("test", shepr_protocol::BUILD_ID);
        let value = serde_json::to_value(server_status_json(&test_paths(), &server))
            .expect("test precondition");
        assert!(value.get("capabilities").is_none());
        assert_eq!(value["presence"], "running");
        assert_eq!(value["boot_id"], "4242-1700000000");
        assert!(value.get("status").is_none());
        assert!(value.get("running").is_none());
    }

    #[test]
    fn every_presence_is_reported_by_name() {
        let status = runtime_status("test", shepr_protocol::BUILD_ID);
        for (server, name) in [
            (ServerPresence::Gone, "gone"),
            (ServerPresence::Starting(status.clone()), "starting"),
            (ServerPresence::Running(status.clone()), "running"),
            (ServerPresence::Stopping(status), "stopping"),
            (ServerPresence::Unresponsive, "unresponsive"),
        ] {
            let value = serde_json::to_value(server_status_json(&test_paths(), &server))
                .expect("test precondition");
            assert_eq!(value["presence"], name);
        }
    }

    #[test]
    fn human_status_detects_a_restart_for_starting_but_not_stopping_servers() {
        let other = runtime_status("0.0.0-old", "ffffffffffffffff");
        let starting = ServerPresence::Starting(other.clone());
        let json = server_status_json(&test_paths(), &starting);
        assert_eq!(json.presence(), ServerPresenceJson::Starting);
        assert_eq!(
            json.identity().map(|identity| identity.boot_id.as_str()),
            Some("4242-1700000000")
        );
        let (compatible, restart_needed) = build_status_flags(&starting);
        assert_eq!(compatible, Some(false));
        assert!(restart_needed);
        assert_eq!(restart_needed_label(&starting, restart_needed), "yes");

        let stopping = ServerPresence::Stopping(other);
        let json = server_status_json(&test_paths(), &stopping);
        assert_eq!(json.presence(), ServerPresenceJson::Stopping);
        let (_, restart_needed) = build_status_flags(&stopping);
        assert!(!restart_needed);
        assert_eq!(restart_needed_label(&stopping, restart_needed), "no");
    }

    #[test]
    fn an_unresponsive_server_has_no_identity() {
        let server = ServerPresence::Unresponsive;
        let json = server_status_json(&test_paths(), &server);
        assert_eq!(json.presence(), ServerPresenceJson::Unresponsive);
        assert_eq!(json.identity(), None);
        let (_, restart_needed) = build_status_flags(&server);
        assert!(!restart_needed);
        assert_eq!(restart_needed_label(&server, restart_needed), "unknown");
    }

    #[test]
    fn same_build_does_not_require_restart() {
        let server = running_server("0.0.0-old", shepr_protocol::BUILD_ID);

        assert_eq!(build_status_flags(&server), (Some(true), false));
    }

    #[test]
    fn different_build_requires_restart() {
        let server = running_server(shepr_protocol::build_version().as_str(), "ffffffffffffffff");

        assert_eq!(build_status_flags(&server), (Some(false), true));
    }
}
