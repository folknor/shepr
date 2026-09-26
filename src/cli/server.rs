use clap::ArgMatches;

use crate::api::schema::{EmptyParams, Method, Request};

pub(super) fn run_server_command(
    matches: &ArgMatches,
    paths: &super::target::CliContext,
) -> std::io::Result<i32> {
    match matches.subcommand() {
        None => Ok(super::missing_subcommand()),
        Some(("stop", _)) => server_stop(paths),
        Some(("agent-manifests", matches)) => {
            server_agent_manifests(paths, super::matches::flag(matches, "json"))
        }
        Some(("reload-agent-manifests", _)) => server_reload_agent_manifests(paths),
        Some(_) => Ok(super::missing_subcommand()),
    }
}

/// The local path skips the protocol check on purpose, like `session stop`:
/// the protocol-mismatch error tells the user to run this command, so it must
/// be able to stop a server from another build.
fn server_stop(paths: &super::target::CliContext) -> std::io::Result<i32> {
    if paths.is_remote() {
        return super::send_ok_request(paths, Method::ServerStop(EmptyParams::default()));
    }

    match crate::session::stop_active_server(paths) {
        Ok(()) => Ok(0),
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn server_agent_manifests(paths: &super::target::CliContext, json: bool) -> std::io::Result<i32> {
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

fn server_reload_agent_manifests(paths: &super::target::CliContext) -> std::io::Result<i32> {
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
