use std::cell::RefCell;
use std::io;
use std::ops::Deref;

use shepr_api::client::ApiClient;
use shepr_remote::machine::{EndpointCatalog, SavedSshEndpoint};

struct MachineTarget {
    profile: SavedSshEndpoint,
    bridge: Option<MachineBridge>,
    ssh_settings: shepr_remote::SavedSshSettings,
}

struct MachineBridge {
    bridge: shepr_remote::SavedSshApiBridge,
    // The status applies to this bridge; replacing the bridge starts unchecked.
    build_checked: bool,
}

#[expect(
    variant_size_differences,
    reason = "a CLI process holds one target; boxing the local flag would only move one byte to the heap"
)]
enum ApiTarget {
    Local { build_checked: bool },
    Machine(Box<MachineTarget>),
}

pub(super) struct CliContext {
    paths: shepr_config::AppPaths,
    target: RefCell<ApiTarget>,
}

impl CliContext {
    /// A context for the local server.
    pub(super) fn local(paths: shepr_config::AppPaths) -> Self {
        Self {
            paths,
            target: RefCell::new(ApiTarget::Local {
                build_checked: false,
            }),
        }
    }

    fn machine(
        paths: shepr_config::AppPaths,
        profile: SavedSshEndpoint,
        ssh_settings: shepr_remote::SavedSshSettings,
    ) -> Self {
        Self {
            paths,
            target: RefCell::new(ApiTarget::Machine(Box::new(MachineTarget {
                profile,
                bridge: None,
                ssh_settings,
            }))),
        }
    }

    pub(super) fn build_checked(&self) -> bool {
        match &*self.target.borrow() {
            ApiTarget::Local { build_checked } => *build_checked,
            ApiTarget::Machine(target) => target
                .bridge
                .as_ref()
                .is_some_and(|bridge| bridge.build_checked),
        }
    }

    pub(super) fn mark_build_checked(&self) {
        match &mut *self.target.borrow_mut() {
            ApiTarget::Local { build_checked } => *build_checked = true,
            ApiTarget::Machine(target) => {
                if let Some(bridge) = target.bridge.as_mut() {
                    bridge.build_checked = true;
                }
            }
        }
    }

    pub(super) fn is_remote(&self) -> bool {
        matches!(&*self.target.borrow(), ApiTarget::Machine(_))
    }
}

impl Deref for CliContext {
    type Target = shepr_config::AppPaths;

    fn deref(&self) -> &Self::Target {
        &self.paths
    }
}

pub(super) fn run_on_machine(
    selector: &str,
    command: Option<&super::CliCommand>,
    paths: &shepr_config::AppPaths,
) -> super::CliResult<i32> {
    let Some(command) = command else {
        return usage_error(&format!("usage: shepr --machine {selector} <command>"));
    };
    if let Err(error) = validate_machine_command(command) {
        return usage_error(&error);
    }
    let config = super::load_validated_config(paths)?;
    let ssh_settings = shepr_remote::SavedSshSettings {
        manage_ssh_config: config.remote().manage_ssh_config,
    };
    let profiles = EndpointCatalog::load_profiles(paths).map_err(io::Error::other)?;
    let profile = match resolve_machine(&profiles, selector) {
        Ok(profile) => profile.clone(),
        Err(error) => return usage_error(&error),
    };
    let context = CliContext::machine(paths.clone(), profile, ssh_settings);
    super::dispatch(command, &context)
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
        let bridge = shepr_remote::SavedSshApiBridge::start(
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
        })?;
        warn_metadata_store_failure(&target.profile, &bridge);
        target.bridge = Some(MachineBridge {
            bridge,
            build_checked: false,
        });
    }
    let bridge = target
        .bridge
        .as_ref()
        .ok_or_else(|| io::Error::other("machine bridge unavailable"))?;
    Ok(ApiClient::for_socket(bridge.bridge.socket_path()))
}

pub(super) fn server_status(
    context: &CliContext,
    client: &ApiClient,
) -> Result<shepr_api::RuntimeStatus, shepr_api::client::ApiClientError> {
    let probe = || {
        if context.is_remote() {
            client.status_with_timeout(shepr_core::limits::SSH_ROUND_TRIP_TIMEOUT)
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
    let retried: Result<(), shepr_api::client::ApiClientError> = {
        let mut target = context.target.borrow_mut();
        let ApiTarget::Machine(target) = &mut *target else {
            return Err(error);
        };
        let Some(bridge) = target.bridge.as_ref() else {
            return Err(error);
        };
        let Some(failure) = bridge.bridge.reported_failure() else {
            return Err(error);
        };
        if !bridge.bridge.used_cached_metadata
            || !shepr_remote::SavedSshApiBridge::stale_metadata_failure(&failure)
        {
            return Err(failure.into());
        }
        // The retry below rediscovers regardless, so this command still recovers. A
        // hint that could not be removed is overwritten by the retry's own store;
        // only when that store fails too does the stale hint outlive this command,
        // and the retry's store failure is reported below.
        if let Err(error) = bridge.bridge.invalidate_metadata() {
            eprintln!(
                "warning: machine '{}': could not remove stale SSH metadata {}: {error}",
                target.profile.label,
                bridge.bridge.metadata_path().display()
            );
        }
        target.bridge.take();
        let bridge = shepr_remote::SavedSshApiBridge::start(
            context,
            &target.profile.id,
            &target.profile.target,
            &target.profile.session,
            false,
            target.ssh_settings,
        )?;
        warn_metadata_store_failure(&target.profile, &bridge);
        target.bridge = Some(MachineBridge {
            bridge,
            build_checked: false,
        });
        Ok(())
    };
    retried?;
    probe()
}

/// A bridge that had to discover the remote shepr and could not remember it still
/// serves this command, but every later command pays discovery again. The CLI has
/// no log subscriber, so this goes to stderr, leaving stdout to the command.
fn warn_metadata_store_failure(
    profile: &SavedSshEndpoint,
    bridge: &shepr_remote::SavedSshApiBridge,
) {
    if let Some(error) = bridge.metadata_store_failure() {
        eprintln!(
            "warning: machine '{}': could not cache the remote shepr location in {}: {error}; \
             later commands rediscover it",
            profile.label,
            bridge.metadata_path().display()
        );
    }
}

pub(super) fn remote_error(context: &CliContext, error: io::Error) -> io::Error {
    let target = context.target.borrow();
    let ApiTarget::Machine(target) = &*target else {
        return error;
    };
    let error = target
        .bridge
        .as_ref()
        .and_then(|bridge| bridge.bridge.reported_failure())
        .unwrap_or(error);
    // Keep the typed SSH failure so callers can still tell authentication,
    // host-key and link failures apart after the machine name is added.
    let diagnostic = shepr_remote::SshFailureDiagnostic::from_error(&error).with_context(format!(
        "machine '{}' (session {})",
        target.profile.label, target.profile.session
    ));
    io::Error::new(error.kind(), diagnostic)
}

pub(super) fn restart_guidance(context: &CliContext) -> String {
    match &*context.target.borrow() {
        ApiTarget::Machine(target) => shepr_api::guidance::operator_guidance(
            shepr_api::guidance::OperatorGuidance::MachineBuildMismatch {
                label: &target.profile.label,
                session: &target.profile.session,
                id: target.profile.id.as_str(),
            },
        ),
        ApiTarget::Local { .. } => shepr_api::session::restart_after_update_guidance_for(context),
    }
}

pub(super) fn remote_identity(context: &CliContext) -> Option<(String, String)> {
    match &*context.target.borrow() {
        ApiTarget::Machine(target) => Some((
            target.profile.id.to_string(),
            target.profile.session.clone(),
        )),
        ApiTarget::Local { .. } => None,
    }
}

pub(super) fn socket_label(context: &CliContext) -> String {
    match remote_identity(context) {
        Some((id, session)) => format!("machine:{id}/{session}"),
        None => shepr_api::socket_path(context).display().to_string(),
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

/// Command eligibility comes from `CliCommand::can_run_on_machine`; this
/// boundary provides the shared rejection message when that predicate refuses.
fn validate_machine_command(command: &super::CliCommand) -> Result<(), String> {
    if command.can_run_on_machine() {
        Ok(())
    } else {
        let command_path = match command.subcommand_name() {
            Some(subcommand) => format!("{} {subcommand}", command.name()),
            None => command.name().to_owned(),
        };
        Err(format!(
            "`{command_path}` cannot run against a machine; --machine only supports noninteractive server API commands and does not run local management commands or attach a TUI",
        ))
    }
}

#[cfg(test)]
impl CliContext {
    pub(super) fn test_local(paths: shepr_config::AppPaths) -> Self {
        Self::local(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_fixtures::*;

    fn parse(values: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
        let mut argv = vec!["shepr"];
        argv.extend_from_slice(values);
        super::super::spec::command().try_get_matches_from(argv)
    }

    #[test]
    fn machine_prefix_routes_without_consuming_command_payload() {
        for prefix in [&["--machine", "mac"][..], &["--machine=mac"]] {
            let mut input = prefix.to_vec();
            input.extend_from_slice(&["session", "stop", "--", "--machine"]);
            let matches = parse(&input).expect("test precondition");
            assert_eq!(
                super::super::matches::string(&matches, "machine").as_deref(),
                Some("mac")
            );
            let Some(("session", session)) = matches.subcommand() else {
                panic!("session command did not parse");
            };
            let Some(("stop", stop)) = session.subcommand() else {
                panic!("session stop did not parse");
            };
            assert_eq!(
                super::super::matches::required(stop, "name").as_deref(),
                Some("--machine")
            );
        }

        let matches =
            parse(&["session", "stop", "--", "--machine=mac"]).expect("test precondition");
        assert_eq!(super::super::matches::string(&matches, "machine"), None);
    }

    #[test]
    fn machine_guidance_offers_a_separate_session_and_the_forced_stop() {
        let guidance = shepr_api::guidance::operator_guidance(
            shepr_api::guidance::OperatorGuidance::MachineBuildMismatch {
                label: "mac",
                session: "agents",
                id: "m1",
            },
        );
        let session = guidance
            .find("--remote-session <name>")
            .expect("the separate-session option is offered");
        let stop = guidance
            .find("`shepr --machine m1 server stop --force`")
            .expect("the forced stop is named");
        assert!(session < stop, "{guidance}");
        assert!(guidance.contains("session agents"), "{guidance}");
    }

    #[test]
    fn machine_prefix_rejects_missing_target_and_conflicting_global_options() {
        for input in [
            &["--machine"][..],
            &["--machine="],
            &["--machine", "--help"],
            &["--machine", "mac", "--machine", "other", "status"],
            &["--machine", "mac", "--session", "other", "status"],
            &["--session", "other", "--machine", "mac", "status"],
            &["--machine", "mac", "--version"],
        ] {
            assert!(parse(input).is_err(), "{input:?}");
        }

        // A machine with no command to run is a usage error before any
        // catalog or network access.
        let error = run_on_machine("mac", None, &shepr_config::AppPaths::test_default())
            .expect_err("a missing command is a usage error");
        assert_eq!(error.exit_code(), 2);
        assert!(matches!(
            error,
            super::super::CliError::Usage(message)
                if message == "usage: shepr --machine mac <command>"
        ));
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

    /// Parse a real CLI command through the production invocation parser so
    /// these cases reach `validate_machine_command` rather than failing clap.
    fn machine_cli_command(command: &[&str]) -> super::super::CliCommand {
        let mut argv = vec!["shepr".to_string(), "--machine".into(), "mac".into()];
        argv.extend(command.iter().map(ToString::to_string));
        let invocation = super::super::parse_invocation(&argv)
            .unwrap_or_else(|code| panic!("{command:?} should parse (exit {code})"));
        match invocation.launch {
            super::super::Launch::Cli(command) => *command,
            _ => panic!("{command:?} should parse as a CLI command"),
        }
    }

    fn machine_command_allowed(command: &[&str]) -> bool {
        let command = machine_cli_command(command);
        validate_machine_command(&command).is_ok()
    }

    #[test]
    fn machine_commands_reject_real_local_commands() {
        for command in [
            &["machine", "remove", "mac"][..],
            &["machine", "list"],
            &["session", "list"],
            &["session", "delete", "default"],
            &[
                "detect",
                "explain",
                "--file",
                "screen.txt",
                "--agent",
                "claude",
            ],
            &["integration", "install", "pi"],
            &["integration", "status"],
            &["status", "client"],
        ] {
            assert!(!machine_command_allowed(command), "{command:?}");
        }
        for command in [
            &["detect", "capture", "w4:p1"][..],
            &["detect", "explain", "w4:p1"],
            &["status"],
            &["status", "server"],
            &["server", "stop"],
        ] {
            assert!(machine_command_allowed(command), "{command:?}");
        }
    }

    #[test]
    fn machine_refusal_formats_commands_without_a_subcommand_cleanly() {
        let command = machine_cli_command(&["status", "client"]);
        let error = validate_machine_command(&command).expect_err("client status is local");
        assert_eq!(
            error,
            "`status client` cannot run against a machine; --machine only supports noninteractive server API commands and does not run local management commands or attach a TUI"
        );

        let overview = machine_cli_command(&["status"]);
        assert_eq!(overview.subcommand_name(), None);
        assert!(overview.can_run_on_machine());
    }

    #[test]
    fn machine_session_attach_is_rejected_as_a_tui_launch() {
        let argv = ["shepr", "--machine", "mac", "session", "attach", "work"].map(str::to_owned);
        let invocation = super::super::parse_invocation(&argv)
            .expect("session attach should parse as a TUI launch");
        assert_eq!(invocation.machine().as_deref(), Some("mac"));
        assert!(matches!(
            &invocation.launch,
            super::super::Launch::Tui {
                attached_session: Some(name)
            } if name == "work"
        ));

        // The command handed to the machine runner comes from the parsed
        // invocation, as in `main`: a TUI launch has none, which is a usage
        // error before any catalog or network access.
        assert!(invocation.cli_command().is_none());
        let error = run_on_machine(
            "mac",
            invocation.cli_command(),
            &shepr_config::AppPaths::test_default(),
        )
        .expect_err("a TUI launch has no API command to run on a machine");
        assert_eq!(error.exit_code(), 2);
    }
}
