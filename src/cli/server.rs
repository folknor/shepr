use clap::ArgMatches;

use crate::api::schema::{EmptyParams, Method, Request};

/// `None` for bare `shepr server`, which runs the headless server.
pub(super) fn run_server_command(matches: &ArgMatches) -> std::io::Result<Option<i32>> {
    match matches.subcommand() {
        None => Ok(None),
        Some(("stop", _)) => server_stop().map(Some),
        Some(("agent-manifests", matches)) => {
            server_agent_manifests(super::matches::flag(matches, "json")).map(Some)
        }
        Some(("reload-agent-manifests", _)) => server_reload_agent_manifests().map(Some),
        Some(_) => Ok(Some(super::missing_subcommand())),
    }
}

fn server_stop() -> std::io::Result<i32> {
    if super::target::is_remote() {
        return super::send_ok_request(Method::ServerStop(EmptyParams::default()));
    }

    match crate::session::stop_active_server() {
        Ok(()) => Ok(0),
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn server_agent_manifests(json: bool) -> std::io::Result<i32> {
    let response = super::send_request(&Request {
        id: "cli:server:agent-manifests".into(),
        method: Method::ServerAgentManifests(EmptyParams::default()),
    })?;
    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }

    print_agent_manifest_status(&response);
    Ok(0)
}

fn server_reload_agent_manifests() -> std::io::Result<i32> {
    super::print_response(&super::send_request(&Request {
        id: "cli:server:reload-agent-manifests".into(),
        method: Method::ServerReloadAgentManifests(EmptyParams::default()),
    })?)
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
