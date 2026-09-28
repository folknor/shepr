use shepr_api::schema::{EmptyParams, Method, Request};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Stop { force: bool },
    AgentManifests { json: bool },
    ReloadAgentManifests,
    Invalid,
}

impl Command {
    pub(super) fn name(self) -> Option<&'static str> {
        match self {
            Self::Stop { .. } => Some("stop"),
            Self::AgentManifests { .. } => Some("agent-manifests"),
            Self::ReloadAgentManifests => Some("reload-agent-manifests"),
            Self::Invalid => None,
        }
    }

    pub(super) fn can_run_on_machine(self) -> bool {
        matches!(
            self,
            Self::Stop { .. } | Self::AgentManifests { .. } | Self::ReloadAgentManifests
        )
    }
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Command {
    match matches.subcommand() {
        Some(("stop", command)) => Command::Stop {
            force: super::matches::flag(command, "force"),
        },
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
        Command::Stop { force } => server_stop(paths, force),
        Command::AgentManifests { json } => server_agent_manifests(paths, json),
        Command::ReloadAgentManifests => server_reload_agent_manifests(paths),
        Command::Invalid => Ok(super::missing_subcommand()),
    }
}

/// Both paths skip the per-command build check, like `session stop`: the
/// build-mismatch error tells the user to stop the server, so this must be
/// able to stop a server from another build. That holds for `--machine` too,
/// where an in-place upgrade of the remote binary leaves the previous server
/// running. It is not silent, though: a server of another build is stopped
/// only with `--force`, because the one a dev build reaches without
/// `--session` is the installed server with every live pane in it.
fn server_stop(paths: &super::target::CliContext, force: bool) -> super::CliResult<i32> {
    if paths.is_remote() {
        if !force {
            let client = super::target::api_client(paths)?;
            let status = super::target::server_status(paths, &client).map_err(|error| {
                super::target::remote_error(paths, super::api_client_error_to_io(error))
            })?;
            let (machine, _) = super::target::remote_identity(paths).unwrap_or_default();
            if let Err(error) = shepr_api::session::guard_mismatched_stop(
                &format!("the server on machine '{machine}'"),
                &shepr_api::session::StopTargetBuild::from_running_build(&status.build_id),
                false,
                &format!(
                    "shepr --machine {machine} server stop {}",
                    shepr_api::session::FORCE_STOP_FLAG
                ),
            ) {
                return Err(super::CliError::Session(
                    super::error::SessionCliError::Stop(error),
                ));
            }
        }
        let response = super::send_request_unchecked(
            paths,
            &Request {
                id: "cli:server:stop".into(),
                method: Method::ServerStop(EmptyParams::default()),
            },
        )?;
        return Ok(if super::print_response_error(&response)? {
            1
        } else {
            0
        });
    }

    // Reported like `session stop` and the remote refusal above.
    shepr_api::session::stop_active_server(paths, force)
        .map_err(|error| super::CliError::Session(super::error::SessionCliError::Stop(error)))?;
    Ok(0)
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
