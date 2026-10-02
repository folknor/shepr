use serde::Serialize;

use shepr_api::schema::{ClientStatusJson, ServerPresenceJson, ServerStatusJson};
use shepr_api::{RuntimeStatus, ServerPresence};
use shepr_remote::{COMMAND_CLIENT, COMMAND_SERVER, FLAG_JSON, option_name_from_flag};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Overview { json: bool },
    Server { json: bool },
    Client { json: bool },
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<Command> {
    let root_json = super::matches::try_flag(matches, option_name_from_flag(FLAG_JSON)).ok()?;
    match matches.subcommand() {
        None => Some(Command::Overview { json: root_json }),
        Some((COMMAND_SERVER, scope)) => {
            let command_json =
                super::matches::try_flag(scope, option_name_from_flag(FLAG_JSON)).ok()?;
            Some(Command::Server {
                json: root_json || command_json,
            })
        }
        Some((COMMAND_CLIENT, scope)) => {
            let command_json =
                super::matches::try_flag(scope, option_name_from_flag(FLAG_JSON)).ok()?;
            Some(Command::Client {
                json: root_json || command_json,
            })
        }
        Some(_) => None,
    }
}

pub(super) fn run_status_command(
    command: Command,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    match command {
        Command::Overview { json } => print_full_status(paths, json),
        Command::Server { json } => print_server_status(paths, json),
        Command::Client { json } => {
            print_client_status(json)?;
            Ok(0)
        }
    }
}

type ServerRuntimeStatus = ServerPresence;

fn print_full_status(paths: &super::target::CliContext, json: bool) -> super::CliResult<i32> {
    let server = read_server_runtime_status(paths)?;

    if json {
        print_json(&FullStatusJson {
            local_client: client_status_json(),
            server: server_status_json(paths, &server),
            update: update_status_json(&server),
        })?;
        return Ok(0);
    }

    println!("local client:");
    print_client_status_body(&client_status_json(), "  ");
    println!();
    println!("server:");
    print_server_status_body(paths, &server, "  ");
    println!();
    println!("update:");
    println!("  restart_needed: {}", restart_needed_label(&server));

    Ok(0)
}

fn print_server_status(paths: &super::target::CliContext, json: bool) -> super::CliResult<i32> {
    let server = read_server_runtime_status(paths)?;
    if json {
        print_json(&server_status_json(paths, &server))?;
        return Ok(0);
    }
    print_server_status_body(paths, &server, "");
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
    println!(
        "{indent}version: {}",
        option_label(status.version.as_deref())
    );
    println!(
        "{indent}build_id: {}",
        option_label(status.build_id.as_deref())
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
    match &server.error {
        Some(error) => println!("{indent}server_error: {error}"),
        None => {
            println!(
                "{indent}server_version: {}",
                option_label(server.version.as_deref())
            );
            println!(
                "{indent}server_build_id: {}",
                option_label(server.build_id.as_deref())
            );
        }
    }
}

fn print_server_status_body(
    paths: &super::target::CliContext,
    server: &ServerRuntimeStatus,
    indent: &str,
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
            build_compatible_label(server)
        );
    }
    println!("{indent}socket: {}", super::target::socket_label(paths));
}

/// The identity a server answered with: present for starting, running and
/// stopping servers, absent for a gone or unresponsive one.
fn answered_status(server: &ServerRuntimeStatus) -> Option<&RuntimeStatus> {
    match server {
        ServerPresence::Starting(status)
        | ServerPresence::Running(status)
        | ServerPresence::Stopping(status) => Some(status),
        ServerPresence::Gone | ServerPresence::Unresponsive => None,
    }
}

fn print_runtime_identity(status: &RuntimeStatus, indent: &str) {
    println!(
        "{indent}version: {}",
        option_label(status.version.as_deref())
    );
    println!("{indent}build_id: {}", status.build_id);
    println!("{indent}boot_id: {}", status.boot_id);
}

fn read_server_runtime_status(
    paths: &super::target::CliContext,
) -> super::CliResult<ServerRuntimeStatus> {
    Ok(shepr_api::read_server_presence_at(
        paths.server_address().socket(),
        crate::limits::STATUS_ANSWER_TIMEOUT,
    )?)
}

fn option_label(value: Option<&str>) -> &str {
    value.unwrap_or("unknown")
}

fn restart_needed_label(server: &ServerRuntimeStatus) -> &'static str {
    match server {
        ServerPresence::Unresponsive => "unknown",
        _ if restart_needed_bool(server) => "yes",
        _ => "no",
    }
}

#[derive(Serialize)]
struct FullStatusJson {
    local_client: ClientStatusJson,
    server: ServerStatusJson,
    update: UpdateStatusJson,
}

#[derive(Serialize)]
struct UpdateStatusJson {
    restart_needed: bool,
}

fn client_status_json() -> ClientStatusJson {
    ClientStatusJson {
        version: Some(shepr_protocol::build_version()),
        build_id: Some(shepr_protocol::BUILD_ID.to_owned()),
        binary: Some(current_exe_label()),
        server: Some(shepr_remote::local_server::sibling_server_status()),
    }
}

fn server_status_json(
    paths: &super::target::CliContext,
    server: &ServerRuntimeStatus,
) -> ServerStatusJson {
    let presence = match server {
        ServerPresence::Gone => ServerPresenceJson::Gone,
        ServerPresence::Starting(_) => ServerPresenceJson::Starting,
        ServerPresence::Running(_) => ServerPresenceJson::Running,
        ServerPresence::Stopping(_) => ServerPresenceJson::Stopping,
        ServerPresence::Unresponsive => ServerPresenceJson::Unresponsive,
    };
    let status = answered_status(server);
    ServerStatusJson {
        presence,
        version: status.and_then(|status| status.version.clone()),
        build_id: status.map(|status| status.build_id.clone()),
        boot_id: status.map(|status| status.boot_id.clone()),
        compatible: build_compatible_bool(server),
        socket: paths.server_address().socket().display().to_string(),
        restart_needed: restart_needed_bool(server),
    }
}

fn update_status_json(server: &ServerRuntimeStatus) -> UpdateStatusJson {
    UpdateStatusJson {
        restart_needed: restart_needed_bool(server),
    }
}

fn build_compatible_label(server: &ServerRuntimeStatus) -> &'static str {
    match build_compatible_bool(server) {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn build_compatible_bool(server: &ServerRuntimeStatus) -> Option<bool> {
    answered_status(server).map(|status| shepr_protocol::is_this_build(&status.build_id))
}

/// A starting or running server of another build needs a restart; a stopping
/// one is already going away, and the successor is launched from this install.
fn restart_needed_bool(server: &ServerRuntimeStatus) -> bool {
    match server {
        ServerPresence::Starting(status) | ServerPresence::Running(status) => {
            !shepr_protocol::is_this_build(&status.build_id)
        }
        ServerPresence::Stopping(_) | ServerPresence::Unresponsive | ServerPresence::Gone => false,
    }
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
    use shepr_test_fixtures::*;

    fn runtime_status(version: Option<&str>, build_id: &str) -> RuntimeStatus {
        RuntimeStatus {
            version: version.map(str::to_owned),
            build_id: build_id.to_owned(),
            boot_id: "4242-1700000000".to_owned(),
            stopping: false,
            starting: false,
        }
    }

    fn running_server(version: Option<&str>, build_id: &str) -> ServerRuntimeStatus {
        ServerPresence::Running(runtime_status(version, build_id))
    }

    fn test_paths() -> super::super::target::CliContext {
        super::super::target::CliContext::test_local(shepr_config::AppPaths::test_default())
    }

    #[test]
    fn server_status_json_reports_the_running_boot() {
        let server = running_server(Some("test"), shepr_protocol::BUILD_ID);
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
        let status = runtime_status(None, shepr_protocol::BUILD_ID);
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
    fn a_starting_server_of_another_build_needs_a_restart_and_a_stopping_one_does_not() {
        let other = runtime_status(Some("0.0.0-old"), "ffffffffffffffff");
        let starting = ServerPresence::Starting(other.clone());
        let json = server_status_json(&test_paths(), &starting);
        assert_eq!(json.presence, ServerPresenceJson::Starting);
        assert_eq!(json.boot_id.as_deref(), Some("4242-1700000000"));
        assert_eq!(json.compatible, Some(false));
        assert!(json.restart_needed);
        assert_eq!(restart_needed_label(&starting), "yes");

        let stopping = ServerPresence::Stopping(other);
        let json = server_status_json(&test_paths(), &stopping);
        assert_eq!(json.presence, ServerPresenceJson::Stopping);
        assert!(!json.restart_needed);
        assert_eq!(restart_needed_label(&stopping), "no");
    }

    #[test]
    fn an_unresponsive_server_has_no_identity() {
        let server = ServerPresence::Unresponsive;
        let json = server_status_json(&test_paths(), &server);
        assert_eq!(json.presence, ServerPresenceJson::Unresponsive);
        assert_eq!(json.version, None);
        assert_eq!(json.build_id, None);
        assert_eq!(json.boot_id, None);
        assert_eq!(json.compatible, None);
        assert!(!json.restart_needed);
        assert_eq!(restart_needed_label(&server), "unknown");
    }

    #[test]
    fn same_build_does_not_require_restart() {
        let server = running_server(Some("0.0.0-old"), shepr_protocol::BUILD_ID);

        assert!(!restart_needed_bool(&server));
        assert_eq!(build_compatible_bool(&server), Some(true));
    }

    #[test]
    fn different_build_requires_restart() {
        let server = running_server(
            Some(shepr_protocol::build_version().as_str()),
            "ffffffffffffffff",
        );

        assert!(restart_needed_bool(&server));
        assert_eq!(build_compatible_bool(&server), Some(false));
    }
}
