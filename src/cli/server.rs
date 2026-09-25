use crate::api::schema::{EmptyParams, Method, Request};

pub(super) fn run_server_command(args: &[String]) -> std::io::Result<Option<i32>> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return Ok(None);
    };

    match subcommand {
        "stop" => server_stop(&args[1..]).map(Some),
        "agent-manifests" => server_agent_manifests(&args[1..]).map(Some),
        "reload-agent-manifests" => server_reload_agent_manifests(&args[1..]).map(Some),
        "help" | "--help" | "-h" => {
            print_server_help();
            Ok(Some(0))
        }
        _ => {
            print_server_help();
            Ok(Some(2))
        }
    }
}

fn server_stop(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: shepr server stop");
        return Ok(2);
    }

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

fn server_agent_manifests(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: shepr server agent-manifests [--json]");
            return Ok(2);
        }
    };

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

fn server_reload_agent_manifests(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: shepr server reload-agent-manifests");
        return Ok(2);
    }

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

fn print_server_help() {
    eprintln!("shepr server commands:");
    eprintln!("  shepr server                run as headless server");
    eprintln!("  shepr server stop           stop the running server via the API socket");
    eprintln!("  shepr server agent-manifests [--json]  show agent detection manifest status");
    eprintln!(
        "  shepr server reload-agent-manifests  reload agent detection manifests in the running server"
    );
}
