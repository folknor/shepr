use std::cell::{Cell, RefCell};
use std::io;
use std::ops::Deref;

use shepr_api::client::ApiClient;
use shepr_remote::machine::{EndpointCatalog, SavedSshEndpoint};

struct MachineTarget {
    profile: SavedSshEndpoint,
    bridge: Option<shepr_remote::SavedSshApiBridge>,
    ssh_settings: shepr_remote::SavedSshSettings,
}

enum ApiTarget {
    Local,
    Machine(Box<MachineTarget>),
}

pub(super) struct CliContext {
    paths: shepr_config::AppPaths,
    target: RefCell<ApiTarget>,
    build_checked: Cell<bool>,
    caller_pane_id: Option<String>,
    caller_socket: Option<std::path::PathBuf>,
}

impl CliContext {
    /// A context for the local server, capturing the calling pane from the
    /// environment once.
    ///
    /// # Errors
    ///
    /// A `SHEPR_PANE_ID` or `SHEPR_SOCKET_PATH` the environment policy refuses.
    pub(super) fn local(paths: shepr_config::AppPaths) -> io::Result<Self> {
        use shepr_core::env::{EnvVar, read_path, read_text};
        Ok(Self {
            paths,
            target: RefCell::new(ApiTarget::Local),
            build_checked: Cell::new(false),
            caller_pane_id: read_text(EnvVar::SheprPaneId)?,
            caller_socket: read_path(EnvVar::SheprSocketPath)?,
        })
    }

    #[cfg(test)]
    pub(super) fn test_local(paths: shepr_config::AppPaths) -> Self {
        Self {
            paths,
            target: RefCell::new(ApiTarget::Local),
            build_checked: Cell::new(false),
            caller_pane_id: None,
            caller_socket: None,
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
            build_checked: Cell::new(false),
            caller_pane_id: None,
            caller_socket: None,
        }
    }

    pub(super) fn build_checked(&self) -> bool {
        self.build_checked.get()
    }

    pub(super) fn mark_build_checked(&self) {
        self.build_checked.set(true);
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
        return usage_error("usage: shepr --machine <label-or-id> <command>");
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
        target.bridge = Some(bridge);
    }
    let bridge = target
        .bridge
        .as_ref()
        .ok_or_else(|| io::Error::other("machine bridge unavailable"))?;
    Ok(ApiClient::for_socket(bridge.socket_path()))
}

pub(super) fn server_status(
    context: &CliContext,
    client: &ApiClient,
) -> Result<shepr_api::RuntimeStatus, shepr_api::client::ApiClientError> {
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
    let retried: Result<(), shepr_api::client::ApiClientError> = {
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
            || !shepr_remote::SavedSshApiBridge::stale_metadata_failure(&failure)
        {
            return Err(failure.into());
        }
        // The retry below rediscovers regardless, so this command still recovers. A
        // hint that could not be removed is overwritten by the retry's own store;
        // only when that store fails too does the stale hint outlive this command,
        // and the retry's store failure is reported below.
        if let Err(error) = bridge.invalidate_metadata() {
            eprintln!(
                "warning: machine '{}': could not remove stale SSH metadata {}: {error}",
                target.profile.label,
                bridge.metadata_path().display()
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
        target.bridge = Some(bridge);
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
        .and_then(shepr_remote::SavedSshApiBridge::reported_failure)
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
        ApiTarget::Machine(target) => machine_restart_guidance(
            &target.profile.label,
            &target.profile.session,
            target.profile.id.as_str(),
        ),
        ApiTarget::Local => shepr_api::session::restart_after_update_guidance_for(context),
    }
}

/// The saved-machine form of the mismatch guidance: like the local one, the
/// way that keeps the running server and its panes comes first, and the stop
/// it names is the forced one a mismatched stop requires.
fn machine_restart_guidance(label: &str, session: &str, id: &str) -> String {
    format!(
        "Install the same Shepr build on machine '{label}'. To keep its running server (session {session}) and its panes, save the machine again with a session of its own: `shepr machine add <ssh-target> --label <label> --remote-session <name>`. To replace that server instead, stop it with `shepr --machine {id} server stop {}`; the next connection starts it again. Stopping the server exits its pane processes.",
        shepr_api::session::FORCE_STOP_FLAG
    )
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
        None => shepr_api::socket_path(context).display().to_string(),
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
        &shepr_api::socket_path(context),
    )
}

fn caller_pane_from(
    pane_id: Option<String>,
    pane_socket: Option<std::path::PathBuf>,
    target_socket: &std::path::Path,
) -> CallerPane {
    let Some(pane_id) = pane_id.filter(|value| !value.trim().is_empty()) else {
        return CallerPane::Unset;
    };
    match pane_socket {
        Some(socket) if socket == target_socket => CallerPane::Known(pane_id),
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

/// Only commands that can run safely against a saved machine are accepted:
/// noninteractive server API requests, without local management, TUI or
/// terminal attachment, or local file evaluation (`agent explain --file`).
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
            assert_eq!(
                super::super::matches::required(prompt, "text").as_deref(),
                Some("--machine")
            );
        }

        let matches =
            parse(&["agent", "prompt", "w4:p1", "--machine=mac"]).expect("test precondition");
        assert_eq!(super::super::matches::string(&matches, "machine"), None);
    }

    #[test]
    fn machine_guidance_offers_a_separate_session_and_the_forced_stop() {
        let guidance = machine_restart_guidance("mac", "agents", "m1");
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
        let error = run_on_machine("mac", None, &shepr_config::AppPaths::test_default())
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
            &["integration", "install", "pi"],
            &["integration", "status"],
            &["config", "check"],
            &["status", "client"],
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
