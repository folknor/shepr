use serde::Serialize;

use shepr_api as api;
use shepr_api::client::ApiClientError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Overview { json: bool },
    Server { json: bool },
    Client { json: bool },
    Invalid,
}

impl Command {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Overview { .. } => "",
            Self::Server { .. } => "server",
            Self::Client { .. } => "client",
            Self::Invalid => "",
        }
    }

    pub(super) fn is_api_command(self) -> bool {
        matches!(self, Self::Server { .. })
    }
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Command {
    let root_json = super::matches::flag(matches, "json");
    match matches.subcommand() {
        None => Command::Overview { json: root_json },
        Some(("server", scope)) => Command::Server {
            json: root_json || super::matches::flag(scope, "json"),
        },
        Some(("client", scope)) => Command::Client {
            json: root_json || super::matches::flag(scope, "json"),
        },
        Some(_) => Command::Invalid,
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
        Command::Invalid => Ok(super::missing_subcommand()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServerRuntimeStatus {
    Running {
        version: Option<String>,
        protocol: Option<u32>,
        capabilities: Option<shepr_api::schema::ServerCapabilities>,
    },
    NotRunning,
}

fn print_full_status(paths: &super::target::CliContext, json: bool) -> super::CliResult<i32> {
    let server = read_server_runtime_status(paths)?;

    if json {
        print_json(&FullStatusJson {
            client: client_status_json(paths),
            server: server_status_json(paths, &server),
            update: update_status_json(&server),
        })?;
        return Ok(0);
    }

    println!("client:");
    println!("  version: {}", shepr_protocol::build_version());
    println!("  protocol: {}", shepr_protocol::PROTOCOL_VERSION);
    println!();
    println!("server:");
    print_server_status_body(paths, &server, "  ");
    println!();
    println!("update:");
    println!("  restart_needed: {}", restart_needed_label(&server));
    println!(
        "  server_binary_stale: {}",
        server_binary_stale_label(&server)
    );

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
    println!("protocol: {}", shepr_protocol::PROTOCOL_VERSION);
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
            version, protocol, ..
        } => {
            println!("{indent}status: running");
            println!("{indent}version: {}", option_label(version.as_deref()));
            println!("{indent}protocol: {}", protocol_label(*protocol));
            println!(
                "{indent}protocol_compatible: {}",
                shepr_protocol::Compatibility::of(*protocol).label()
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
            protocol: status.protocol,
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

fn protocol_label(protocol: Option<u32>) -> String {
    protocol
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn restart_needed_label(server: &ServerRuntimeStatus) -> &'static str {
    match restart_needed_bool(server) {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn server_binary_stale_label(server: &ServerRuntimeStatus) -> &'static str {
    match server_binary_stale_bool(server) {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

#[derive(Serialize)]
struct FullStatusJson {
    client: ClientStatusJson,
    server: ServerStatusJson,
    update: UpdateStatusJson,
}

#[derive(Serialize)]
struct ClientStatusJson {
    version: String,
    protocol: u32,
    binary: String,
    session: Option<String>,
}

#[derive(Serialize)]
struct ServerStatusJson {
    status: &'static str,
    running: bool,
    version: Option<String>,
    protocol: Option<u32>,
    capabilities: Option<ServerCapabilitiesJson>,
    compatible: Option<bool>,
    socket: String,
    session: Option<String>,
    restart_needed: Option<bool>,
    server_binary_stale: Option<bool>,
}

#[derive(Serialize)]
struct ServerCapabilitiesJson {
    detached_server_daemon: bool,
    ssh_agent_registration: bool,
}

#[derive(Serialize)]
struct UpdateStatusJson {
    restart_needed: Option<bool>,
    server_binary_stale: Option<bool>,
}

fn client_status_json(paths: &shepr_config::AppPaths) -> ClientStatusJson {
    ClientStatusJson {
        version: shepr_protocol::build_version(),
        protocol: shepr_protocol::PROTOCOL_VERSION,
        binary: current_exe_label(),
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
            protocol,
            capabilities,
        } => ServerStatusJson {
            status: "running",
            running: true,
            version: version.clone(),
            protocol: *protocol,
            capabilities: capabilities
                .as_ref()
                .map(|capabilities| ServerCapabilitiesJson {
                    detached_server_daemon: capabilities.detached_server_daemon,
                    ssh_agent_registration: capabilities.ssh_agent_registration,
                }),
            compatible: shepr_protocol::Compatibility::of(*protocol).known(),
            socket: api::socket_path(paths).display().to_string(),
            session: paths.session_id().name().map(str::to_owned),
            restart_needed: restart_needed_bool(server),
            server_binary_stale: server_binary_stale_bool(server),
        },
        ServerRuntimeStatus::NotRunning => ServerStatusJson {
            status: "not_running",
            running: false,
            version: None,
            protocol: None,
            capabilities: None,
            compatible: None,
            socket: api::socket_path(paths).display().to_string(),
            session: paths.session_id().name().map(str::to_owned),
            restart_needed: Some(false),
            server_binary_stale: Some(false),
        },
    };
    if let Some((_, session)) = super::target::remote_identity(paths) {
        status.socket = super::target::socket_label(paths);
        status.session = Some(session);
        status.server_binary_stale = None;
    }
    status
}

fn update_status_json(server: &ServerRuntimeStatus) -> UpdateStatusJson {
    // This object is emitted only by the full overview. Machine targets reject
    // that command, so its local version comparison is never paired with the
    // remote server object's intentionally unknown stale-binary value.
    UpdateStatusJson {
        restart_needed: restart_needed_bool(server),
        server_binary_stale: server_binary_stale_bool(server),
    }
}

fn restart_needed_bool(server: &ServerRuntimeStatus) -> Option<bool> {
    match server {
        ServerRuntimeStatus::Running { protocol, .. } => {
            Some(!shepr_protocol::Compatibility::of(*protocol).is_compatible())
        }
        ServerRuntimeStatus::NotRunning => Some(false),
    }
}

fn server_binary_stale_bool(server: &ServerRuntimeStatus) -> Option<bool> {
    match server {
        ServerRuntimeStatus::Running { version, .. } => version
            .as_deref()
            .map(|version| version != shepr_protocol::build_version()),
        ServerRuntimeStatus::NotRunning => Some(false),
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
    shepr_platform::launch_executable()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|err| format!("unknown ({err})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_server(version: Option<&str>, protocol: Option<u32>) -> ServerRuntimeStatus {
        ServerRuntimeStatus::Running {
            version: version.map(str::to_owned),
            protocol,
            capabilities: Some(shepr_api::schema::ServerCapabilities {
                detached_server_daemon: true,
                ssh_agent_registration: false,
            }),
        }
    }

    #[test]
    fn status_exposes_only_dynamic_server_capabilities() {
        let server = running_server(Some("test"), Some(shepr_protocol::PROTOCOL_VERSION));
        let paths = super::super::target::CliContext::test_local(shepr_config::AppPaths::default());
        let value =
            serde_json::to_value(server_status_json(&paths, &server)).expect("test precondition");
        assert_eq!(value["capabilities"]["ssh_agent_registration"], false);
        assert!(value["capabilities"].get("surface_interest").is_none());
        assert!(value["capabilities"].get("health_check").is_none());
    }

    #[test]
    fn stale_compatible_server_does_not_require_restart() {
        let server = running_server(Some("0.0.0-old"), Some(shepr_protocol::PROTOCOL_VERSION));

        assert_eq!(restart_needed_bool(&server), Some(false));
        assert_eq!(server_binary_stale_bool(&server), Some(true));
    }

    #[test]
    fn server_with_other_protocol_requires_restart() {
        let server = running_server(
            Some(shepr_protocol::build_version().as_str()),
            Some(shepr_protocol::PROTOCOL_VERSION + 1),
        );

        assert_eq!(restart_needed_bool(&server), Some(true));
        assert_eq!(server_binary_stale_bool(&server), Some(false));
    }
}
