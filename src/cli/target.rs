use std::cell::RefCell;
use std::io;

use crate::api::client::{ApiClient, ConnectionTarget};
use crate::client::endpoint::{EndpointCatalog, SavedSshEndpoint};

thread_local! {
    // CLI dispatch is synchronous. Scope routing to this command, never the runtime or TUI.
    static TARGET: RefCell<Option<MachineTarget>> = const { RefCell::new(None) };
}

struct MachineTarget {
    profile: SavedSshEndpoint,
    bridge: Option<crate::remote::SavedSshApiBridge>,
}

struct TargetScope(Option<MachineTarget>);

impl Drop for TargetScope {
    fn drop(&mut self) {
        TARGET.with(|target| *target.borrow_mut() = self.0.take());
    }
}

/// Runs `command` (the subcommand parsed after `--machine <selector>`) against
/// the saved machine. The spec already rejects `--machine` combined with other
/// launch options.
pub(super) fn run_on_machine(
    selector: &str,
    command: Option<(&str, &clap::ArgMatches)>,
) -> io::Result<super::CommandOutcome> {
    let Some((name, matches)) = command else {
        return usage_error("usage: shepr --machine <label-or-id> <command>");
    };
    if let Err(error) = validate_machine_command(name, matches) {
        return usage_error(&error);
    }
    let profiles = EndpointCatalog::load_profiles().map_err(io::Error::other)?;
    let profile = match resolve_machine(&profiles, selector) {
        Ok(profile) => profile.clone(),
        Err(error) => return usage_error(&error),
    };
    let _scope = TARGET.with(|target| {
        TargetScope(target.replace(Some(MachineTarget {
            profile,
            bridge: None,
        })))
    });
    super::dispatch(name, matches)
}

fn usage_error(error: &str) -> io::Result<super::CommandOutcome> {
    eprintln!("error: {error}");
    Ok(super::CommandOutcome::Handled(2))
}

pub(super) fn is_remote() -> bool {
    TARGET.with(|target| target.borrow().is_some())
}

pub(super) fn api_client() -> io::Result<ApiClient> {
    TARGET.with(|target| {
        let mut target = target.borrow_mut();
        let Some(target) = target.as_mut() else {
            return Ok(ApiClient::local());
        };
        if target.bridge.is_none() {
            target.bridge = Some(
                crate::remote::SavedSshApiBridge::start(
                    target.profile.id.as_str(),
                    &target.profile.target,
                    &target.profile.session,
                    true,
                )
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("machine '{}': {error}", target.profile.label),
                    )
                })?,
            );
        }
        let bridge = target
            .bridge
            .as_ref()
            .ok_or_else(|| io::Error::other("machine bridge unavailable"))?;
        Ok(ApiClient::for_target(ConnectionTarget::SocketPath(
            bridge.socket_path().to_owned(),
        )))
    })
}

pub(super) fn server_status(
    client: &ApiClient,
) -> Result<crate::api::RuntimeStatus, crate::api::client::ApiClientError> {
    let probe = || {
        if is_remote() {
            client.status_with_timeout(std::time::Duration::from_secs(15))
        } else {
            client.status()
        }
    };
    let error = match probe() {
        Ok(status) => return Ok(status),
        Err(error) => error,
    };
    // Only this read-only probe may rediscover and retry. Requests that follow
    // the probe must never be replayed after an ambiguous SSH failure.
    TARGET.with(|target| {
        let mut target = target.borrow_mut();
        let Some(target) = target.as_mut() else {
            return Err(error);
        };
        let Some(bridge) = target.bridge.as_ref() else {
            return Err(error);
        };
        let Some(failure) = bridge.reported_failure() else {
            return Err(error);
        };
        if !bridge.used_cached_metadata
            || !crate::remote::SavedSshApiBridge::stale_metadata_failure(&failure)
        {
            return Err(failure.into());
        }
        bridge.invalidate_metadata();
        target.bridge.take();
        target.bridge = Some(crate::remote::SavedSshApiBridge::start(
            target.profile.id.as_str(),
            &target.profile.target,
            &target.profile.session,
            false,
        )?);
        Ok(())
    })?;
    probe()
}

pub(super) fn remote_error(error: io::Error) -> io::Error {
    TARGET.with(|target| {
        let target = target.borrow();
        let Some(target) = target.as_ref() else {
            return error;
        };
        let error = target
            .bridge
            .as_ref()
            .and_then(crate::remote::SavedSshApiBridge::reported_failure)
            .unwrap_or(error);
        io::Error::new(
            error.kind(),
            format!(
                "machine '{}' (session {}): {error}",
                target.profile.label, target.profile.session
            ),
        )
    })
}

pub(super) fn restart_guidance() -> String {
    TARGET.with(|target| match target.borrow().as_ref() {
        Some(target) => format!("Update Shepr and restart the server on machine '{}' (session {}). Stopping the server exits its pane processes.", target.profile.label, target.profile.session),
        None => crate::session::active_restart_after_update_guidance(),
    })
}

pub(super) fn remote_identity() -> Option<(String, String)> {
    TARGET.with(|target| {
        target.borrow().as_ref().map(|target| {
            (
                target.profile.id.to_string(),
                target.profile.session.clone(),
            )
        })
    })
}

pub(super) fn socket_label() -> String {
    match remote_identity() {
        Some((id, session)) => format!("machine:{id}/{session}"),
        None => crate::api::socket_path().display().to_string(),
    }
}

pub(super) fn caller_pane_id() -> Option<String> {
    if is_remote() {
        return None;
    }
    std::env::var("SHEPR_PANE_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

pub(super) fn resolve_machine<'a>(
    profiles: &'a [SavedSshEndpoint],
    selector: &str,
) -> Result<&'a SavedSshEndpoint, String> {
    let profile = if let Some(profile) = profiles
        .iter()
        .find(|profile| profile.id.as_str() == selector)
    {
        profile
    } else {
        let mut matches = profiles.iter().filter(|profile| profile.label == selector);
        let profile = matches
            .next()
            .ok_or_else(|| format!("unknown machine '{selector}'; use `shepr machine list`"))?;
        if matches.next().is_some() {
            return Err(format!(
                "machine label '{selector}' is ambiguous; use its profile ID"
            ));
        }
        profile
    };
    if !profile.enabled {
        return Err(format!("machine '{selector}' is disabled"));
    }
    Ok(profile)
}

/// Only commands that are pure API requests may run against a saved machine:
/// no local side effects (config, sessions, integrations, machine catalog), no
/// TUI or terminal attach, and no local file evaluation (`agent explain --file`).
fn validate_machine_command(command: &str, matches: &clap::ArgMatches) -> Result<(), String> {
    let (subcommand, local_file) = match matches.subcommand() {
        Some((name, sub_matches)) => (name, super::matches::string(sub_matches, "file").is_some()),
        None => ("", false),
    };
    let supported = match command {
        "workspace" | "tab" | "pane" => true,
        "agent" => subcommand != "attach" && !(subcommand == "explain" && local_file),
        "status" => subcommand == "server",
        "server" => matches!(
            subcommand,
            "stop" | "agent-manifests" | "reload-agent-manifests"
        ),
        _ => false,
    };
    if supported {
        Ok(())
    } else {
        Err(format!(
            "`{command} {subcommand}` is not an API-backed machine command; --machine does not run local management commands or attach a TUI"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(values: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
        let mut argv = vec!["shepr"];
        argv.extend_from_slice(values);
        super::super::spec::command().try_get_matches_from(argv)
    }

    #[test]
    fn machine_prefix_routes_without_consuming_command_payload() {
        for prefix in [&["--machine", "mac"][..], &["--machine=mac"]] {
            let mut input = prefix.to_vec();
            input.extend_from_slice(&["agent", "prompt", "w4:p1", "--machine"]);
            let matches = parse(&input).expect("test precondition");
            assert_eq!(
                super::super::matches::string(&matches, "machine").as_deref(),
                Some("mac")
            );
            let Some(("agent", agent)) = matches.subcommand() else {
                panic!("agent command did not parse");
            };
            let Some(("prompt", prompt)) = agent.subcommand() else {
                panic!("agent prompt did not parse");
            };
            assert_eq!(super::super::matches::required(prompt, "text"), "--machine");
        }

        let matches =
            parse(&["agent", "prompt", "w4:p1", "--machine=mac"]).expect("test precondition");
        assert_eq!(super::super::matches::string(&matches, "machine"), None);
    }

    #[test]
    fn machine_prefix_rejects_missing_target_and_conflicting_global_options() {
        for input in [
            &["--machine"][..],
            &["--machine="],
            &["--machine", "--help"],
            &["--machine", "mac", "--machine", "other", "agent", "list"],
            &["--machine", "mac", "--session", "other", "agent", "list"],
            &["--session", "other", "--machine", "mac", "agent", "list"],
            &["--remote", "other", "--machine", "mac", "agent", "list"],
            &["--machine", "mac", "--version"],
        ] {
            assert!(parse(input).is_err(), "{input:?}");
        }

        // A machine with no command to run is a usage error before any
        // catalog or network access.
        let outcome = run_on_machine("mac", None).expect("test precondition");
        assert!(matches!(outcome, super::super::CommandOutcome::Handled(2)));
    }

    #[test]
    fn machine_resolution_requires_a_unique_enabled_saved_machine() {
        let mac = SavedSshEndpoint::new("mac", "mac-ssh", "agents").expect("test precondition");
        let other =
            SavedSshEndpoint::new("build", "builder", "default").expect("test precondition");
        let profiles = vec![mac.clone(), other];
        assert_eq!(
            resolve_machine(&profiles, "mac").expect("test precondition"),
            &mac
        );
        assert_eq!(
            resolve_machine(&profiles, mac.id.as_str()).expect("test precondition"),
            &mac
        );
        let shadow =
            SavedSshEndpoint::new(mac.id.as_str(), "shadow", "default").expect("test precondition");
        assert_eq!(
            resolve_machine(&[mac.clone(), shadow], mac.id.as_str()).expect("test precondition"),
            &mac
        );
        assert!(resolve_machine(&profiles, "mac-ssh").is_err());
        assert!(resolve_machine(&profiles, "missing").is_err());
        let duplicate =
            SavedSshEndpoint::new("mac", "other", "default").expect("test precondition");
        assert!(resolve_machine(&[mac.clone(), duplicate], "mac").is_err());
        let mut disabled = mac;
        disabled.enabled = false;
        assert!(resolve_machine(&[disabled], "mac").is_err());
    }

    /// Whether `shepr --machine mac <command>` would reach the network: it
    /// must parse, and then pass the API-only check.
    fn machine_command_allowed(command: &[&str]) -> bool {
        let mut input = vec!["--machine", "mac"];
        input.extend_from_slice(command);
        let Ok(matches) = parse(&input) else {
            return false;
        };
        let Some((name, matches)) = matches.subcommand() else {
            return false;
        };
        validate_machine_command(name, matches).is_ok()
    }

    #[test]
    fn machine_commands_reject_local_side_effects_and_tui_attach() {
        for command in [
            &["update"][..],
            &["machine", "remove", "mac"],
            &["session", "delete", "default"],
            &["session", "attach", "work"],
            &["agent", "attach", "w4:p1"],
            &[
                "agent",
                "explain",
                "--file",
                "screen.txt",
                "--agent",
                "claude",
            ],
            &["terminal", "attach", "w4:p1"],
            &["terminal", "session", "control", "w4:p1"],
            &["plugin", "install", "./plugin"],
            &["integration", "install", "pi"],
            &["config", "check"],
            &["api", "schema", "--output", "schema.json"],
            // There is no `api` command; this used to pass the check and then
            // exit 0 without doing anything.
            &["api", "snapshot"],
            &["status", "client"],
            &["status"],
            &["server"],
            &["client"],
            &["remote-api-bridge"],
        ] {
            assert!(!machine_command_allowed(command), "{command:?}");
        }
        for command in [
            &["agent", "list"][..],
            &["agent", "wait", "w4:p1"],
            &["agent", "explain", "w4:p1"],
            &["pane", "split", "w4:p1", "--direction", "right"],
            &["workspace", "list"],
            &["tab", "list"],
            &["status", "server"],
            &["server", "stop"],
        ] {
            assert!(machine_command_allowed(command), "{command:?}");
        }
    }
}
