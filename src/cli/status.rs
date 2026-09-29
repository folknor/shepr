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

impl Command {
    pub(super) fn name(self) -> Option<&'static str> {
        match self {
            Self::Overview { .. } => None,
            Self::Server { .. } => Some(COMMAND_SERVER),
            Self::Client { .. } => Some(COMMAND_CLIENT),
        }
    }

    pub(super) fn can_run_on_machine(self) -> bool {
        match self {
            Self::Overview { .. } | Self::Server { .. } => true,
            Self::Client { .. } => false,
        }
    }
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<Command> {
    let root_json = super::matches::flag(matches, option_name_from_flag(FLAG_JSON));
    match matches.subcommand() {
        None => Some(Command::Overview { json: root_json }),
        Some((COMMAND_SERVER, scope)) => Some(Command::Server {
            json: root_json || super::matches::flag(scope, option_name_from_flag(FLAG_JSON)),
        }),
        Some((COMMAND_CLIENT, scope)) => Some(Command::Client {
            json: root_json || super::matches::flag(scope, option_name_from_flag(FLAG_JSON)),
        }),
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
            print_client_status(json, paths)?;
            Ok(0)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServerRuntimeStatus {
    Running {
        version: Option<String>,
        build_id: String,
        capabilities: Option<shepr_api::schema::ServerCapabilities>,
    },
    NotRunning,
}

fn print_full_status(paths: &super::target::CliContext, json: bool) -> super::CliResult<i32> {
    let server = read_server_runtime_status(paths)?;

    if json {
        print_json(&FullStatusJson {
            local_client: client_status_json(paths),
            server: server_status_json(paths, &server),
            update: update_status_json(&server),
        })?;
        return Ok(0);
    }

    println!("local client:");
    println!("  version: {}", shepr_protocol::build_version());
    println!("  build_id: {}", shepr_protocol::BUILD_ID);
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

fn print_client_status(json: bool, paths: &shepr_config::AppPaths) -> super::CliResult<()> {
    if json {
        print_json(&client_status_json(paths))?;
        return Ok(());
    }

    println!("version: {}", shepr_protocol::build_version());
    println!("build_id: {}", shepr_protocol::BUILD_ID);
    println!("binary: {}", current_exe_label());
    Ok(())
}

fn print_server_status_body(
    paths: &super::target::CliContext,
    server: &ServerRuntimeStatus,
    indent: &str,
) {
    match server {
        ServerRuntimeStatus::Running {
            version, build_id, ..
        } => {
            println!("{indent}status: running");
            println!("{indent}version: {}", option_label(version.as_deref()));
            println!("{indent}build_id: {build_id}");
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
    let client = super::target::api_client(paths)?;
    match super::target::server_status(paths, &client) {
        Ok(status) => Ok(ServerRuntimeStatus::Running {
            version: status.version,
            build_id: status.build_id,
            capabilities: status.capabilities,
        }),
        Err(err) if paths.is_remote() => {
            Err(super::target::remote_error(paths, super::api_client_error_to_io(err)).into())
        }
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

fn client_status_json(paths: &shepr_config::AppPaths) -> ClientStatusJson {
    ClientStatusJson {
        version: Some(shepr_protocol::build_version()),
        build_id: Some(shepr_protocol::BUILD_ID.to_owned()),
        binary: Some(current_exe_label()),
        session: paths.session_id().name().map(str::to_owned),
    }
}

fn server_status_json(
    paths: &super::target::CliContext,
    server: &ServerRuntimeStatus,
) -> ServerStatusJson {
    let mut status = match server {
        ServerRuntimeStatus::Running {
            version,
            build_id,
            capabilities,
        } => ServerStatusJson {
            running: true,
            version: version.clone(),
            build_id: Some(build_id.clone()),
            capabilities: capabilities.clone(),
            compatible: build_compatible_bool(server),
            socket: api::socket_path(paths).display().to_string(),
            session: paths.session_id().name().map(str::to_owned),
            restart_needed: restart_needed_bool(server),
        },
        ServerRuntimeStatus::NotRunning => ServerStatusJson {
            running: false,
            version: None,
            build_id: None,
            capabilities: None,
            compatible: None,
            socket: api::socket_path(paths).display().to_string(),
            session: paths.session_id().name().map(str::to_owned),
            restart_needed: false,
        },
    };
    if let Some((_, session)) = super::target::remote_identity(paths) {
        status.socket = super::target::socket_label(paths);
        status.session = Some(session);
    }
    status
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
            capabilities: Some(shepr_api::schema::ServerCapabilities {
                detached_server_daemon: true,
                ssh_agent_registration: false,
            }),
        }
    }

    #[test]
    fn status_exposes_only_dynamic_server_capabilities() {
        let server = running_server(Some("test"), shepr_protocol::BUILD_ID);
        let paths =
            super::super::target::CliContext::test_local(shepr_config::AppPaths::test_default());
        let value =
            serde_json::to_value(server_status_json(&paths, &server)).expect("test precondition");
        assert_eq!(
            value["capabilities"],
            serde_json::json!({
                "detached_server_daemon": true,
                "ssh_agent_registration": false,
            })
        );
        assert_eq!(value["running"], true);
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
