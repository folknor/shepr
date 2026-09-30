use serde::Serialize;

use shepr_api as api;
use shepr_api::client::ApiClientError;
use shepr_api::schema::{ClientStatusJson, ServerStatusJson};
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServerRuntimeStatus {
    Running {
        version: Option<String>,
        build_id: String,
        boot_id: String,
    },
    NotRunning,
}

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
    match server {
        ServerRuntimeStatus::Running {
            version,
            build_id,
            boot_id,
        } => {
            println!("{indent}status: running");
            println!("{indent}version: {}", option_label(version.as_deref()));
            println!("{indent}build_id: {build_id}");
            println!("{indent}boot_id: {boot_id}");
            println!(
                "{indent}build_compatible: {}",
                build_compatible_label(server)
            );
            println!("{indent}socket: {}", super::target::socket_label(paths));
        }
        ServerRuntimeStatus::NotRunning => {
            println!("{indent}status: not running");
            println!("{indent}socket: {}", super::target::socket_label(paths));
        }
    }
}

fn read_server_runtime_status(
    paths: &super::target::CliContext,
) -> super::CliResult<ServerRuntimeStatus> {
    let client = super::target::api_client(paths);
    match client.status() {
        Ok(status) => Ok(ServerRuntimeStatus::Running {
            version: status.version,
            build_id: status.build_id,
            boot_id: status.boot_id,
        }),
        Err(ApiClientError::Io(error)) => {
            match super::server_not_running_error(&client.socket_path()) {
                Ok(true) => Ok(ServerRuntimeStatus::NotRunning),
                Ok(false) => Err(error.into()),
                Err(probe_error) => Err(probe_error),
            }
        }
        Err(err) => Err(super::api_client_error_to_io(err).into()),
    }
}

fn option_label(value: Option<&str>) -> &str {
    value.unwrap_or("unknown")
}

fn restart_needed_label(server: &ServerRuntimeStatus) -> &'static str {
    if restart_needed_bool(server) {
        "yes"
    } else {
        "no"
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
    match server {
        ServerRuntimeStatus::Running {
            version,
            build_id,
            boot_id,
        } => ServerStatusJson {
            running: true,
            version: version.clone(),
            build_id: Some(build_id.clone()),
            boot_id: Some(boot_id.clone()),
            compatible: build_compatible_bool(server),
            socket: api::socket_path(paths).display().to_string(),
            restart_needed: restart_needed_bool(server),
        },
        ServerRuntimeStatus::NotRunning => ServerStatusJson {
            running: false,
            version: None,
            build_id: None,
            boot_id: None,
            compatible: None,
            socket: api::socket_path(paths).display().to_string(),
            restart_needed: false,
        },
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
    match server {
        ServerRuntimeStatus::Running { build_id, .. } => {
            Some(shepr_protocol::is_this_build(build_id))
        }
        ServerRuntimeStatus::NotRunning => None,
    }
}

fn restart_needed_bool(server: &ServerRuntimeStatus) -> bool {
    match server {
        ServerRuntimeStatus::Running { build_id, .. } => !shepr_protocol::is_this_build(build_id),
        ServerRuntimeStatus::NotRunning => false,
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

    fn running_server(version: Option<&str>, build_id: &str) -> ServerRuntimeStatus {
        ServerRuntimeStatus::Running {
            version: version.map(str::to_owned),
            build_id: build_id.to_owned(),
            boot_id: "4242-1700000000".to_owned(),
        }
    }

    #[test]
    fn server_status_json_reports_the_running_boot() {
        let server = running_server(Some("test"), shepr_protocol::BUILD_ID);
        let paths =
            super::super::target::CliContext::test_local(shepr_config::AppPaths::test_default());
        let value =
            serde_json::to_value(server_status_json(&paths, &server)).expect("test precondition");
        assert!(value.get("capabilities").is_none());
        assert_eq!(value["running"], true);
        assert_eq!(value["boot_id"], "4242-1700000000");
        assert!(value.get("status").is_none());
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
