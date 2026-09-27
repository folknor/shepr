use std::cell::{Cell, RefCell};
use std::io;
use std::ops::Deref;

use crate::api::client::{ApiClient, ConnectionTarget};
use crate::machine::{EndpointCatalog, SavedSshEndpoint};

struct MachineTarget {
    profile: SavedSshEndpoint,
    bridge: Option<crate::remote::SavedSshApiBridge>,
    ssh_settings: crate::remote::SavedSshSettings,
}

enum ApiTarget {
    Local,
    Machine(Box<MachineTarget>),
}

pub(super) struct CliContext {
    paths: crate::config::AppPaths,
    target: RefCell<ApiTarget>,
    protocol_checked: Cell<bool>,
    caller_pane_id: Option<String>,
    caller_socket: Option<std::ffi::OsString>,
}

impl CliContext {
    pub(super) fn local(paths: crate::config::AppPaths) -> Self {
        Self {
            paths,
            target: RefCell::new(ApiTarget::Local),
            protocol_checked: Cell::new(false),
            caller_pane_id: std::env::var(crate::integration::SHEPR_PANE_ID_ENV_VAR).ok(),
            caller_socket: std::env::var_os(crate::config::SOCKET_PATH_ENV_VAR),
        }
    }

    #[cfg(test)]
    pub(super) fn test_local(paths: crate::config::AppPaths) -> Self {
        Self {
            paths,
            target: RefCell::new(ApiTarget::Local),
            protocol_checked: Cell::new(false),
            caller_pane_id: None,
            caller_socket: None,
        }
    }

    fn machine(
        paths: crate::config::AppPaths,
        profile: SavedSshEndpoint,
        ssh_settings: crate::remote::SavedSshSettings,
    ) -> Self {
        Self {
            paths,
            target: RefCell::new(ApiTarget::Machine(Box::new(MachineTarget {
                profile,
                bridge: None,
                ssh_settings,
            }))),
            protocol_checked: Cell::new(false),
            caller_pane_id: None,
            caller_socket: None,
        }
    }

    pub(super) fn protocol_checked(&self) -> bool {
        self.protocol_checked.get()
    }

    pub(super) fn mark_protocol_checked(&self) {
        self.protocol_checked.set(true);
    }

    pub(super) fn is_remote(&self) -> bool {
        matches!(&*self.target.borrow(), ApiTarget::Machine(_))
    }
}

impl Deref for CliContext {
    type Target = crate::config::AppPaths;

    fn deref(&self) -> &Self::Target {
        &self.paths
    }
}

pub(super) fn run_on_machine(
    selector: &str,
    command: Option<&super::CliCommand>,
    paths: &crate::config::AppPaths,
) -> super::CliResult<i32> {
    let Some(command) = command else {
        return usage_error("usage: shepr --machine <label-or-id> <command>");
    };
    if let Err(error) = validate_machine_command(command) {
        return usage_error(&error);
    }
    let config = super::load_validated_config(paths)?;
    let ssh_settings = crate::remote::SavedSshSettings {
        manage_ssh_config: config.remote.manage_ssh_config,
    };
    let profiles = EndpointCatalog::load_profiles(paths).map_err(io::Error::other)?;
    let profile = match resolve_machine(&profiles, selector) {
        Ok(profile) => profile.clone(),
        Err(error) => return usage_error(&error),
    };
    let context = CliContext::machine(paths.clone(), profile, ssh_settings);
    super::dispatch_with_config(command, Some(config), &context)
}

fn usage_error(error: &str) -> super::CliResult<i32> {
    Err(super::CliError::Usage(error.into()))
}

pub(super) fn api_client(context: &CliContext) -> super::CliResult<ApiClient> {
    let mut target = context.target.borrow_mut();
    let ApiTarget::Machine(target) = &mut *target else {
        return Ok(ApiClient::local(context));
    };
    if target.bridge.is_none() {
        target.bridge = Some(
            crate::remote::SavedSshApiBridge::start(
                context,
                &target.profile.id,
                &target.profile.target,
                &target.profile.session,
                true,
                target.ssh_settings,
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
}

pub(super) fn server_status(
    context: &CliContext,
    client: &ApiClient,
) -> Result<crate::api::RuntimeStatus, crate::api::client::ApiClientError> {
    let probe = || {
        if context.is_remote() {
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
    let retried: Result<(), crate::api::client::ApiClientError> = {
        let mut target = context.target.borrow_mut();
        let ApiTarget::Machine(target) = &mut *target else {
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
            context,
            &target.profile.id,
            &target.profile.target,
            &target.profile.session,
            false,
            target.ssh_settings,
        )?);
        Ok(())
    };
    retried?;
    probe()
}

pub(super) fn remote_error(context: &CliContext, error: io::Error) -> io::Error {
    let target = context.target.borrow();
    let ApiTarget::Machine(target) = &*target else {
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
}

pub(super) fn restart_guidance(context: &CliContext) -> String {
    match &*context.target.borrow() {
        ApiTarget::Machine(target) => format!(
            "Update Shepr and restart the server on machine '{}' (session {}). Stopping the server exits its pane processes.",
            target.profile.label, target.profile.session
        ),
        ApiTarget::Local => crate::session::restart_after_update_guidance_for(context),
    }
}

pub(super) fn remote_identity(context: &CliContext) -> Option<(String, String)> {
    match &*context.target.borrow() {
        ApiTarget::Machine(target) => Some((
            target.profile.id.to_string(),
            target.profile.session.clone(),
        )),
        ApiTarget::Local => None,
    }
}

pub(super) fn socket_label(context: &CliContext) -> String {
    match remote_identity(context) {
        Some((id, session)) => format!("machine:{id}/{session}"),
        None => crate::api::socket_path(context).display().to_string(),
    }
}

/// The pane this CLI process runs in, as far as the targeted server is
/// concerned.
///
/// Every pane's environment carries `SHEPR_PANE_ID` together with
/// `SHEPR_SOCKET_PATH`, the API socket of the server that owns the pane. The
/// pane id only means something to that server, so it is used only when the
/// command goes to the same socket: `--session` naming another session, a
/// socket override that differs from the pane's, or `--machine` all make the
/// id foreign. (A pane shell that re-exports `SHEPR_SOCKET_PATH` itself
/// cannot be told apart from the pane's own value; nothing else records which
/// server a pane belongs to.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CallerPane {
    /// The command runs in this pane of the targeted server.
    Known(String),
    /// `SHEPR_PANE_ID` is unset: the command does not run inside a pane.
    Unset,
    /// `SHEPR_PANE_ID` names a pane of a different server than the target.
    OtherServer,
    /// `--machine`: the caller's pane can never be on that machine's server.
    Remote,
}

impl CallerPane {
    /// The pane id when it belongs to the targeted server.
    pub(super) fn id(&self) -> Option<String> {
        match self {
            Self::Known(pane_id) => Some(pane_id.clone()),
            Self::Unset | Self::OtherServer | Self::Remote => None,
        }
    }

    /// The pane id for `--current`, which has no fallback.
    pub(super) fn require(&self) -> Result<String, String> {
        match self {
            Self::Known(pane_id) => Ok(pane_id.clone()),
            Self::Unset => Err(
                "--current needs the calling pane, but SHEPR_PANE_ID is not set; run it inside a shepr pane or name the pane with --pane"
                    .into(),
            ),
            Self::OtherServer => Err(
                "--current names the calling pane, which belongs to a different server than this command targets (--session or SHEPR_SOCKET_PATH); name the pane with --pane"
                    .into(),
            ),
            Self::Remote => Err(
                "--current cannot be used with --machine: the calling pane is not on that machine; name the pane with --pane"
                    .into(),
            ),
        }
    }
}

pub(super) fn caller_pane(context: &CliContext) -> CallerPane {
    if context.is_remote() {
        return CallerPane::Remote;
    }
    caller_pane_from(
        context.caller_pane_id.clone(),
        context.caller_socket.clone(),
        &crate::api::socket_path(context),
    )
}

fn caller_pane_from(
    pane_id: Option<String>,
    pane_socket: Option<std::ffi::OsString>,
    target_socket: &std::path::Path,
) -> CallerPane {
    let Some(pane_id) = pane_id.filter(|value| !value.trim().is_empty()) else {
        return CallerPane::Unset;
    };
    match pane_socket {
        Some(socket) if std::path::Path::new(&socket) == target_socket => {
            CallerPane::Known(pane_id)
        }
        _ => CallerPane::OtherServer,
    }
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
    Ok(profile)
}

/// Only commands that are pure API requests may run against a saved machine:
/// no local side effects (config, sessions, integrations, machine catalog), no
/// TUI or terminal attach, and no local file evaluation (`agent explain --file`).
fn validate_machine_command(command: &super::CliCommand) -> Result<(), String> {
    if command.is_api_command() {
        Ok(())
    } else {
        Err(format!(
            "`{} {}` is not an API-backed machine command; --machine does not run local management commands or attach a TUI",
            command.name(),
            command.subcommand_name(),
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
    fn caller_pane_is_known_only_on_the_pane_s_own_server() {
        let own = std::path::Path::new("/run/shepr/shepr.sock");
        assert_eq!(
            caller_pane_from(Some("w1:p2".into()), Some(own.into()), own),
            CallerPane::Known("w1:p2".into())
        );
        // `--session other` or a different socket override.
        let other = std::path::Path::new("/run/shepr/sessions/other/shepr.sock");
        assert_eq!(
            caller_pane_from(Some("w1:p2".into()), Some(own.into()), other),
            CallerPane::OtherServer
        );
        // A pane id without the socket it belongs to cannot be placed.
        assert_eq!(
            caller_pane_from(Some("w1:p2".into()), None, own),
            CallerPane::OtherServer
        );
        assert_eq!(
            caller_pane_from(None, Some(own.into()), own),
            CallerPane::Unset
        );
        assert_eq!(
            caller_pane_from(Some("  ".into()), Some(own.into()), own),
            CallerPane::Unset
        );

        assert_eq!(CallerPane::Known("p".into()).id().as_deref(), Some("p"));
        assert_eq!(CallerPane::Known("p".into()).require().as_deref(), Ok("p"));
        for unknown in [
            CallerPane::Unset,
            CallerPane::OtherServer,
            CallerPane::Remote,
        ] {
            assert_eq!(unknown.id(), None);
            assert!(unknown.require().is_err());
        }
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
        let error = run_on_machine("mac", None, &crate::config::AppPaths::default())
            .expect_err("a missing command is a usage error");
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn machine_resolution_requires_a_unique_saved_machine() {
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
        let Some(command) = super::super::CliCommand::from_matches(name, matches) else {
            return false;
        };
        validate_machine_command(&command).is_ok()
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
