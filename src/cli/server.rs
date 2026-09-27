use shepr_api::schema::{EmptyParams, Method, Request};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Stop,
    AgentManifests { json: bool },
    ReloadAgentManifests,
    Invalid,
}

impl Command {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::AgentManifests { .. } => "agent-manifests",
            Self::ReloadAgentManifests => "reload-agent-manifests",
            Self::Invalid => "",
        }
    }

    pub(super) fn is_api_command(self) -> bool {
        matches!(
            self,
            Self::Stop | Self::AgentManifests { .. } | Self::ReloadAgentManifests
        )
    }
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Command {
    match matches.subcommand() {
        Some(("stop", _)) => Command::Stop,
        Some(("agent-manifests", command)) => Command::AgentManifests {
            json: super::matches::flag(command, "json"),
        },
        Some(("reload-agent-manifests", _)) => Command::ReloadAgentManifests,
        _ => Command::Invalid,
    }
}

pub(super) fn run_server_command(
    command: Command,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    match command {
        Command::Stop => server_stop(paths),
        Command::AgentManifests { json } => server_agent_manifests(paths, json),
        Command::ReloadAgentManifests => server_reload_agent_manifests(paths),
        Command::Invalid => Ok(super::missing_subcommand()),
    }
}

/// The local path skips the protocol check on purpose, like `session stop`:
/// the protocol-mismatch error tells the user to run this command, so it must
/// be able to stop a server from another build.
fn server_stop(paths: &super::target::CliContext) -> super::CliResult<i32> {
    if paths.is_remote() {
        return super::send_ok_request(paths, Method::ServerStop(EmptyParams::default()));
    }

    match shepr_api::session::stop_active_server(paths) {
        Ok(()) => Ok(0),
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn server_agent_manifests(paths: &super::target::CliContext, json: bool) -> super::CliResult<i32> {
    let response = super::send_request(
        paths,
        &Request {
            id: "cli:server:agent-manifests".into(),
            method: Method::ServerAgentManifests(EmptyParams::default()),
        },
    )?;
    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }

    print_agent_manifest_status(&response);
    Ok(0)
}

fn server_reload_agent_manifests(paths: &super::target::CliContext) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:server:reload-agent-manifests".into(),
            method: Method::ServerReloadAgentManifests(EmptyParams::default()),
        },
    )?)
}

fn print_agent_manifest_status(response: &serde_json::Value) {
    let Some(manifests) = response["result"]["manifests"].as_array() else {
        return;
    };
    for manifest in manifests {
        let agent = manifest["agent"].as_str().unwrap_or("-");
        let source = manifest["source"].as_str().unwrap_or("-");
        println!("{agent:<11} {source}");
        if let Some(warning) = manifest["warning"].as_str() {
            println!("  {warning}");
        }
    }
}
