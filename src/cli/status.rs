use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

use shepr_api::client::{ApiClient, ServerSummary};
use shepr_api::schema::{
    ClientStatusJson, ServerStatusJson, ServerSummaryJson, SiblingServerJson, StatusOverviewJson,
};
use shepr_launch::invocation::{
    COMMAND_CLIENT, COMMAND_SERVER, FLAG_ALL, FLAG_JSON, PROGRAM_NAME, SERVER_BINARY_NAME,
    option_name_from_flag,
};
use shepr_launch::status::{RuntimeStatus, ServerPresence};
use shepr_paths::{BuildProfile, ServerAddress};
use shepr_protocol::BuildIdentity;
use shepr_remote::fleet::MachineStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    /// `all` adds the server of every machine configured in `client.toml`.
    Overview {
        json: bool,
        all: bool,
    },
    Server {
        json: bool,
    },
}

pub(super) enum ParsedCommand {
    Local(Command),
    Client { json: bool },
}

/// The status-specific usage error when `--all` is combined with a scoped
/// subcommand. `parse` and the CLI's typed-parser fallback share this so the
/// deliberate refusal gets a useful operator message.
pub(super) fn all_scope_refusal(matches: &clap::ArgMatches) -> Option<&'static str> {
    if !super::matches::try_flag(matches, option_name_from_flag(FLAG_ALL)).ok()? {
        return None;
    }
    match matches.subcommand() {
        Some((COMMAND_SERVER, _)) => {
            Some("status --all cannot be combined with the 'server' subcommand")
        }
        Some((COMMAND_CLIENT, _)) => {
            Some("status --all cannot be combined with the 'client' subcommand")
        }
        Some(_) => Some("status --all cannot be used with a subcommand"),
        None => None,
    }
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<ParsedCommand> {
    let root_json = super::matches::try_flag(matches, option_name_from_flag(FLAG_JSON)).ok()?;
    let all = super::matches::try_flag(matches, option_name_from_flag(FLAG_ALL)).ok()?;
    // `--all` applies only to the overview. Do not silently drop it when a
    // scoped subcommand is selected.
    if all_scope_refusal(matches).is_some() {
        return None;
    }
    match matches.subcommand() {
        None => Some(ParsedCommand::Local(Command::Overview {
            json: root_json,
            all,
        })),
        Some((COMMAND_SERVER, scope)) => {
            let command_json =
                super::matches::try_flag(scope, option_name_from_flag(FLAG_JSON)).ok()?;
            Some(ParsedCommand::Local(Command::Server {
                json: root_json || command_json,
            }))
        }
        Some((COMMAND_CLIENT, scope)) => {
            let command_json =
                super::matches::try_flag(scope, option_name_from_flag(FLAG_JSON)).ok()?;
            Some(ParsedCommand::Client {
                json: root_json || command_json,
            })
        }
        Some(_) => None,
    }
}

pub(super) fn run_status_command(
    command: Command,
    paths: &shepr_paths::AppPaths,
) -> super::CliResult<i32> {
    match command {
        Command::Overview { json, all } => print_full_status(paths, json, all),
        Command::Server { json } => print_server_status(paths, json),
    }
}

/// `shepr status`, and with `all` every configured machine after it. The
/// machines are read first, so a `client.toml` the TUI would refuse fails the
/// command before any SSH runs; they then answer concurrently.
fn print_full_status(
    paths: &shepr_paths::AppPaths,
    json: bool,
    all: bool,
) -> super::CliResult<i32> {
    let fleet = if all {
        Some(super::fleet::load(paths)?)
    } else {
        None
    };
    let machines = fleet.as_ref().map(|fleet| {
        let statuses = shepr_remote::fleet::on_every_machine(&fleet.machines, |machine| {
            shepr_remote::fleet::machine_status(paths, machine)
        });
        fleet.machines.iter().zip(statuses).collect::<Vec<_>>()
    });
    let server = read_server_runtime_status(paths)?;
    let address = paths.server_address();
    // Only a running server of this build is asked for its counts: another
    // build may not know the method, and a starting one is still restoring.
    let summary = match &server {
        ServerPresence::Running(status) if status.build_id.is_this_build() => {
            Some(read_server_summary(address.socket()))
        }
        _ => None,
    };

    if json {
        let local = StatusOverviewJson {
            local_client: client_status_json(),
            server: server_status_json(paths, &server),
            summary: summary.and_then(Result::ok).map(summary_json),
        };
        match machines {
            None => print_json(&local)?,
            Some(machines) => print_json(&FleetStatusJson {
                local,
                machines: machines
                    .into_iter()
                    .map(|(machine, status)| MachineStatusJson::new(machine, status))
                    .collect(),
            })?,
        }
        return Ok(0);
    }

    let installation = Installation {
        version: shepr_protocol::PACKAGE_VERSION.to_owned(),
        build_id: BuildIdentity::for_this_build(),
        profile: BuildProfile::current(),
        binary: current_exe_label(),
        sibling: shepr_launch::local_server::sibling_server_status(),
    };
    let report_now = SystemTime::now();
    let overview = Overview {
        installation: &installation,
        server: &server,
        summary,
        address,
        mismatch_hint: &shepr_launch::guidance::status_build_mismatch_hint(address),
        now: report_now,
    };
    print!("{}", render_overview(&overview));
    if let Some(machines) = machines {
        print!("{}", render_machines(&machines, paths, report_now));
    }
    Ok(0)
}

/// The machines section of `status --all`: one line per configured machine.
fn render_machines(
    machines: &[(&shepr_config::MachineConfig, std::io::Result<MachineStatus>)],
    paths: &shepr_paths::AppPaths,
    now: SystemTime,
) -> String {
    let mut out = String::from("\n");
    if machines.is_empty() {
        // `status --all` resolves the config paths to read client.toml, so the
        // path is always present here; without it the line names no file.
        let line = paths.client_config_file().map_or_else(
            || "machines: none configured".to_owned(),
            |file| format!("machines: none configured in {}", file.display()),
        );
        push_line(&mut out, &line);
        return out;
    }
    push_line(&mut out, "machines");
    let rows = machines
        .iter()
        .map(|(machine, status)| {
            let text = match status {
                Ok(status) => machine_line(&status.overview, now),
                Err(error) => super::fleet::failure_label(error),
            };
            (machine.label.to_string(), text)
        })
        .collect::<Vec<_>>();
    out.push_str(&super::fleet::render_rows(&rows));
    out
}

/// One machine's server as its own `shepr` reported it: its state, pid and
/// uptime, its counts when it is of that host's build, then the builds when
/// they disagree. A server that is not the build installed beside it was
/// started before an update, and a restart brings the installed one up; an
/// install that is not this `shepr`'s build is one this client cannot use.
fn machine_line(overview: &StatusOverviewJson, now: SystemTime) -> String {
    use shepr_api::schema::ServerStatus;
    let mut line = match &overview.server.state {
        ServerStatus::Gone => "not running".to_owned(),
        ServerStatus::Unresponsive => "not answering".to_owned(),
        ServerStatus::Stopping(identity) => {
            format!("stopping{}", process_facts(&identity.boot_id, None))
        }
        ServerStatus::Starting(identity) => format!(
            "starting{}, restoring panes",
            process_facts(&identity.boot_id, Some(now))
        ),
        ServerStatus::Running(identity) => {
            let mut running = format!("running{}", process_facts(&identity.boot_id, Some(now)));
            if let Some(summary) = overview.summary {
                running.push_str(&format!(
                    "   {}",
                    counts_label(&ServerSummary {
                        workspaces: summary.workspaces,
                        panes: summary.panes,
                        agents: summary.agents,
                        blocked_agents: summary.blocked_agents,
                    })
                ));
            }
            running
        }
    };
    let installed = overview
        .local_client
        .identity
        .as_ref()
        .map(|identity| identity.build_id);
    let server = overview.server.identity().map(|identity| identity.build_id);
    match (server, installed) {
        (Some(server), Some(installed)) if !server.matches(installed) => {
            line.push_str(&format!(
                "; the server is not the installed build (server {}, installed {}): restart it to update",
                short_build(server),
                short_build(installed)
            ));
        }
        (_, Some(installed)) if !installed.is_this_build() => {
            line.push_str(&format!(
                "; installed build {} is not this shepr's",
                short_build(installed)
            ));
        }
        _ => {}
    }
    line
}

/// `status --all --json`: this host's overview, then each configured machine.
#[derive(Serialize)]
struct FleetStatusJson {
    local: StatusOverviewJson,
    machines: Vec<MachineStatusJson>,
}

/// One machine's answer, or why it gave none.
#[derive(Serialize)]
struct MachineStatusJson {
    label: String,
    /// The path of the `shepr` that answered there.
    executable: Option<String>,
    status: Option<StatusOverviewJson>,
    error: Option<String>,
}

impl MachineStatusJson {
    fn new(machine: &shepr_config::MachineConfig, status: std::io::Result<MachineStatus>) -> Self {
        let label = machine.label.to_string();
        match status {
            Ok(status) => Self {
                label,
                executable: Some(status.executable),
                status: Some(status.overview),
                error: None,
            },
            Err(error) => Self {
                label,
                executable: None,
                status: None,
                error: Some(super::fleet::failure_label(&error)),
            },
        }
    }
}

fn print_server_status(paths: &shepr_paths::AppPaths, json: bool) -> super::CliResult<i32> {
    let server = read_server_runtime_status(paths)?;
    if json {
        print_json(&server_status_json(paths, &server))?;
        return Ok(0);
    }
    print_server_status_body(paths, &server, build_compatible(&server));
    Ok(0)
}

pub(super) fn print_client_status(json: bool) -> super::CliResult<()> {
    let status = client_status_json();
    if json {
        print_json(&status)?;
        return Ok(());
    }

    print_client_status_body(&status);
    Ok(())
}

/// The client's identity and the identity of the `shepr-server` installed
/// beside it, which a remote client's discovery checks against its own build.
fn print_client_status_body(status: &ClientStatusJson) {
    let identity = status.identity.as_ref();
    println!(
        "version: {}",
        option_label(identity.map(|identity| identity.version.as_str()))
    );
    println!(
        "build_id: {}",
        identity.map_or_else(
            || "unknown".into(),
            |identity| identity.build_id.to_string()
        )
    );
    if let Some(binary) = status.binary.as_deref() {
        println!("binary: {binary}");
    }
    let Some(server) = status.server.as_ref() else {
        return;
    };
    if let Some(binary) = server.binary.as_deref() {
        println!("server_binary: {binary}");
    }
    match &server.identity {
        Err(error) => println!("server_error: {error}"),
        Ok(identity) => {
            println!("server_version: {}", identity.version);
            println!("server_build_id: {}", identity.build_id);
        }
    }
}

/// The `key: value` form of `shepr status server`.
fn print_server_status_body(
    paths: &shepr_paths::AppPaths,
    server: &ServerPresence,
    compatible: Option<bool>,
) {
    let label = match server {
        ServerPresence::Gone => "not running",
        ServerPresence::Starting(_) => "starting",
        ServerPresence::Running(_) => "running",
        ServerPresence::Stopping(_) => "stopping",
        ServerPresence::Unresponsive => "unresponsive",
    };
    println!("status: {label}");
    if let Some(status) = answered_status(server) {
        println!("version: {}", status.version);
        println!("build_id: {}", status.build_id);
        println!("boot_id: {}", status.boot_id);
    }
    if !matches!(server, ServerPresence::Gone) {
        println!("build_compatible: {}", build_compatible_label(compatible));
    }
    println!("socket: {}", paths.server_address().socket().display());
}

/// The identity a server answered with: present for starting, running and
/// stopping servers, absent for a gone or unresponsive one.
fn answered_status(server: &ServerPresence) -> Option<&RuntimeStatus> {
    match server {
        ServerPresence::Starting(status)
        | ServerPresence::Running(status)
        | ServerPresence::Stopping(status) => Some(status),
        ServerPresence::Gone | ServerPresence::Unresponsive => None,
    }
}

fn read_server_runtime_status(paths: &shepr_paths::AppPaths) -> super::CliResult<ServerPresence> {
    Ok(shepr_launch::status::read_server_presence_at(
        paths.server_address().socket(),
        shepr_launch::limits::STATUS_REQUEST_TIMEOUT,
    )?)
}

/// The running server's counts, or why they could not be read. A failure
/// only leaves the counts out of the report; the status still exits 0.
fn read_server_summary(socket: &Path) -> Result<ServerSummary, String> {
    let deadline = Instant::now()
        .checked_add(shepr_launch::limits::STATUS_SUMMARY_TIMEOUT)
        .ok_or_else(|| "status timeout is too large".to_owned())?;
    ApiClient::for_socket(socket)
        .server_summary_until(deadline)
        .map_err(|error| error.to_string())
}

fn option_label(value: Option<&str>) -> &str {
    value.unwrap_or("unknown")
}

/// This executable and the `shepr-server` installed beside it.
struct Installation {
    version: String,
    build_id: BuildIdentity,
    profile: BuildProfile,
    binary: String,
    sibling: SiblingServerJson,
}

/// Everything the human `shepr status` report shows, read before rendering
/// so the rendering is pure.
struct Overview<'a> {
    installation: &'a Installation,
    server: &'a ServerPresence,
    /// Asked only of a running server of this build.
    summary: Option<Result<ServerSummary, String>>,
    address: &'a ServerAddress,
    /// What to run about a server of another build, spelt for this build's
    /// entry point and the socket override.
    mismatch_hint: &'a str,
    now: SystemTime,
}

fn render_overview(overview: &Overview<'_>) -> String {
    let mut out = String::new();
    render_installation(&mut out, overview.installation);
    out.push('\n');
    render_server(&mut out, overview);
    out
}

fn render_installation(out: &mut String, installation: &Installation) {
    push_line(
        out,
        &format!(
            "{PROGRAM_NAME} {} ({}, build {})",
            installation.version,
            installation.profile.marker(),
            installation.build_id
        ),
    );
    push_line(out, &format!("  {}", installation.binary));
    match &installation.sibling.identity {
        Err(error) => {
            push_line(out, &format!("{SERVER_BINARY_NAME} unusable"));
            push_line(out, &format!("  {error}"));
        }
        Ok(sibling) => {
            let difference = if sibling.version != installation.version {
                "version"
            } else if !installation.build_id.matches(sibling.build_id) {
                "build"
            } else {
                return;
            };
            push_line(
                out,
                &format!(
                    "{SERVER_BINARY_NAME} {} (build {}), a different {difference}",
                    sibling.version,
                    short_build(sibling.build_id)
                ),
            );
            if let Some(binary) = installation.sibling.binary.as_deref() {
                push_line(out, &format!("  {binary}"));
            }
        }
    }
}

fn render_server(out: &mut String, overview: &Overview<'_>) {
    // Compare with the installation identity carried by this view;
    // `is_this_build()` would bypass it and use the process-global build.
    let this_build = overview.installation.build_id;
    match overview.server {
        ServerPresence::Gone => push_line(out, "server: not running"),
        ServerPresence::Unresponsive => push_line(
            out,
            "server: not answering; something listens at the socket but gave no status",
        ),
        ServerPresence::Stopping(status) => push_line(
            out,
            &format!("server: stopping{}", process_facts(&status.boot_id, None)),
        ),
        ServerPresence::Starting(status) | ServerPresence::Running(status)
            if !this_build.matches(status.build_id) =>
        {
            let state = if matches!(overview.server, ServerPresence::Starting(_)) {
                "starting"
            } else {
                "running"
            };
            let identity = if status.version == overview.installation.version {
                short_build(status.build_id)
            } else {
                format!(
                    "version {}, build {}",
                    status.version,
                    short_build(status.build_id)
                )
            };
            push_line(
                out,
                &format!("server: {state} a different build ({identity}), unusable by this shepr"),
            );
            push_line(out, &format!("  {}", overview.mismatch_hint));
        }
        ServerPresence::Starting(status) => push_line(
            out,
            &format!(
                "server: starting{}, restoring panes",
                process_facts(&status.boot_id, Some(overview.now))
            ),
        ),
        ServerPresence::Running(status) => {
            push_line(
                out,
                &format!(
                    "server: running{}",
                    process_facts(&status.boot_id, Some(overview.now))
                ),
            );
            match &overview.summary {
                Some(Ok(summary)) => push_line(out, &format!("  {}", counts_label(summary))),
                Some(Err(error)) => push_line(out, &format!("  counts unavailable: {error}")),
                None => {}
            }
        }
    }
    let socket = overview.address.socket().display();
    if overview.address.is_runtime_address() {
        push_line(out, &format!("  socket  {socket}"));
    } else {
        push_line(
            out,
            &format!(
                "  socket  {socket}, set by {}",
                shepr_core::env::EnvVar::SheprSocketPath
            ),
        );
    }
}

/// `, pid N` and, given the time now, `, up 3h12m`, from what the boot
/// identity records: the server's pid and the wall clock at its start.
fn process_facts(boot_id: &shepr_protocol::BootId, now: Option<SystemTime>) -> String {
    let pid = format!(", pid {}", boot_id.process_id());
    let up = now
        .and_then(|now| uptime(boot_id, now))
        .map_or_else(String::new, |uptime| {
            format!(", up {}", uptime_label(uptime))
        });
    format!("{pid}{up}")
}

/// How long ago the boot's wall clock reading was, if it is not in the future
/// or before the epoch.
fn uptime(boot_id: &shepr_protocol::BootId, now: SystemTime) -> Option<Duration> {
    if boot_id.is_before_epoch() {
        return None;
    }
    let started = SystemTime::UNIX_EPOCH.checked_add(Duration::from_nanos(
        u64::try_from(boot_id.clock_nanos()).ok()?,
    ))?;
    now.duration_since(started).ok()
}

/// The two largest units of `uptime`: `42s`, `17m`, `3h12m`, `2d5h`.
fn uptime_label(uptime: Duration) -> String {
    // limits-exempt: time unit definitions for the uptime label, not bounds.
    const MINUTE: u64 = 60;
    // limits-exempt: time unit definitions for the uptime label, not bounds.
    const HOUR: u64 = 60 * MINUTE;
    // limits-exempt: time unit definitions for the uptime label, not bounds.
    const DAY: u64 = 24 * HOUR;
    let seconds = uptime.as_secs();
    if seconds < MINUTE {
        format!("{seconds}s")
    } else if seconds < HOUR {
        format!("{}m", seconds / MINUTE)
    } else if seconds < DAY {
        format!("{}h{}m", seconds / HOUR, seconds % HOUR / MINUTE)
    } else {
        format!("{}d{}h", seconds / DAY, seconds % DAY / HOUR)
    }
}

fn counts_label(summary: &ServerSummary) -> String {
    let blocked = if summary.blocked_agents == 0 {
        String::new()
    } else {
        format!(" ({} blocked)", summary.blocked_agents)
    };
    format!(
        "workspaces {}   panes {}   agents {}{blocked}",
        summary.workspaces, summary.panes, summary.agents
    )
}

/// The first eight hex digits of a build identity, enough to tell builds
/// apart at a glance.
fn short_build(build_id: BuildIdentity) -> String {
    match build_id {
        BuildIdentity::Unidentifiable => "unidentifiable".to_owned(),
        BuildIdentity::Known(_) => {
            let text = build_id.to_string();
            format!("{}...", text.get(..8).unwrap_or(&text))
        }
    }
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

fn summary_json(summary: ServerSummary) -> ServerSummaryJson {
    ServerSummaryJson {
        workspaces: summary.workspaces,
        panes: summary.panes,
        agents: summary.agents,
        blocked_agents: summary.blocked_agents,
    }
}

fn client_status_json() -> ClientStatusJson {
    ClientStatusJson {
        identity: Some(shepr_protocol::BuildVersion {
            version: shepr_protocol::PACKAGE_VERSION.to_owned(),
            build_id: BuildIdentity::for_this_build(),
        }),
        binary: Some(current_exe_label()),
        server: Some(shepr_launch::local_server::sibling_server_status()),
    }
}

fn server_status_json(paths: &shepr_paths::AppPaths, server: &ServerPresence) -> ServerStatusJson {
    use shepr_api::schema::{ServerIdentity, ServerStatus};
    let identity = |status: &RuntimeStatus| ServerIdentity {
        version: status.version.clone(),
        build_id: status.build_id,
        boot_id: status.boot_id.clone(),
    };
    let state = match server {
        ServerPresence::Gone => ServerStatus::Gone,
        ServerPresence::Starting(status) => ServerStatus::Starting(identity(status)),
        ServerPresence::Running(status) => ServerStatus::Running(identity(status)),
        ServerPresence::Stopping(status) => ServerStatus::Stopping(identity(status)),
        ServerPresence::Unresponsive => ServerStatus::Unresponsive,
    };
    ServerStatusJson {
        state,
        socket: paths.server_address().socket().display().to_string(),
    }
}

fn build_compatible_label(compatible: Option<bool>) -> &'static str {
    match compatible {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

/// Whether the server that answered is of this build; unknown when none did.
fn build_compatible(server: &ServerPresence) -> Option<bool> {
    answered_status(server).map(|status| status.build_id.is_this_build())
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
    // replaced install reports its path, not "/.../shepr (deleted)".
    shepr_platform::launch_executable().map_or_else(
        |err| format!("unknown ({err})"),
        |path| path.display().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_api::schema::ServerPresenceJson;
    use shepr_test_fixtures::*;

    const THIS_BUILD: &str = "38df64ce2909c428";
    const OTHER_BUILD: &str = "5a1c09e2ffffffff";
    /// The wall clock the test boot started at, in seconds since the epoch.
    const BOOTED_AT: u64 = 1_700_000_000;

    fn runtime_status(version: &str, build_id: &str) -> RuntimeStatus {
        RuntimeStatus {
            version: version.to_owned(),
            build_id: build_id.parse().expect("build identity"),
            boot_id: "4242-1700000000".parse().expect("boot identity"),
            lifecycle: shepr_launch::status::RuntimeLifecycle::Running,
        }
    }

    fn running_server(version: &str, build_id: &str) -> ServerPresence {
        ServerPresence::Running(runtime_status(version, build_id))
    }

    fn test_paths() -> shepr_paths::AppPaths {
        shepr_paths::AppPaths::test_default()
    }

    /// A server of `build_id` booted as pid 41233 at [`BOOTED_AT`].
    fn booted(version: &str, build_id: &str) -> RuntimeStatus {
        RuntimeStatus {
            boot_id: shepr_protocol::BootId::from_process_clock(
                41233,
                Ok(Duration::from_secs(BOOTED_AT)),
            ),
            ..runtime_status(version, build_id)
        }
    }

    fn installation(sibling: Result<(&str, &str), &str>) -> Installation {
        Installation {
            version: "0.1.0".into(),
            build_id: THIS_BUILD.parse().expect("build identity"),
            profile: BuildProfile::Release,
            binary: "/nonexistent/shepr/bin/shepr".into(),
            sibling: SiblingServerJson {
                binary: Some("/nonexistent/shepr/bin/shepr-server".into()),
                identity: sibling
                    .map(|(version, build_id)| shepr_protocol::BuildVersion {
                        version: version.into(),
                        build_id: build_id.parse().expect("build identity"),
                    })
                    .map_err(str::to_owned),
            },
        }
    }

    fn runtime_address() -> ServerAddress {
        ServerAddress::for_runtime_dir(Path::new("/run/user/1000/shepr"), None)
            .expect("valid test socket path")
    }

    fn render(
        installation: &Installation,
        server: &ServerPresence,
        summary: Option<Result<ServerSummary, String>>,
        address: &ServerAddress,
    ) -> String {
        render_overview(&Overview {
            installation,
            server,
            summary,
            address,
            mismatch_hint: "run `shepr stop`, then `shepr`",
            now: SystemTime::UNIX_EPOCH + Duration::from_secs(BOOTED_AT + 3 * 3600 + 12 * 60),
        })
    }

    fn summary(blocked_agents: usize) -> ServerSummary {
        ServerSummary {
            workspaces: 3,
            panes: 7,
            agents: 4,
            blocked_agents,
        }
    }

    #[test]
    fn a_stopped_server_reads_as_the_installation_and_its_socket() {
        let installation = installation(Ok(("0.1.0", THIS_BUILD)));
        assert_eq!(
            render(
                &installation,
                &ServerPresence::Gone,
                None,
                &runtime_address()
            ),
            "shepr 0.1.0 (release, build 38df64ce2909c428)\n  /nonexistent/shepr/bin/shepr\n\nserver: not running\n  socket  /run/user/1000/shepr/shepr.sock\n"
        );
    }

    #[test]
    fn a_running_server_of_this_build_shows_its_pid_uptime_and_counts() {
        let installation = installation(Ok(("0.1.0", THIS_BUILD)));
        let server = ServerPresence::Running(booted("0.1.0", THIS_BUILD));
        assert_eq!(
            render(
                &installation,
                &server,
                Some(Ok(summary(1))),
                &runtime_address()
            ),
            "shepr 0.1.0 (release, build 38df64ce2909c428)\n  /nonexistent/shepr/bin/shepr\n\nserver: running, pid 41233, up 3h12m\n  workspaces 3   panes 7   agents 4 (1 blocked)\n  socket  /run/user/1000/shepr/shepr.sock\n"
        );
        let unblocked = render(
            &installation,
            &server,
            Some(Ok(summary(0))),
            &runtime_address(),
        );
        assert!(
            unblocked.contains("\n  workspaces 3   panes 7   agents 4\n"),
            "{unblocked}"
        );
        let unavailable = render(
            &installation,
            &server,
            Some(Err("timed out".into())),
            &runtime_address(),
        );
        assert!(
            unavailable.contains("\n  counts unavailable: timed out\n"),
            "{unavailable}"
        );
    }

    #[test]
    fn a_server_of_another_build_is_named_with_the_restart_hint() {
        let installation = installation(Ok(("0.1.0", THIS_BUILD)));
        let server = ServerPresence::Running(booted("0.1.0", OTHER_BUILD));
        assert_eq!(
            render(&installation, &server, None, &runtime_address()),
            "shepr 0.1.0 (release, build 38df64ce2909c428)\n  /nonexistent/shepr/bin/shepr\n\nserver: running a different build (5a1c09e2...), unusable by this shepr\n  run `shepr stop`, then `shepr`\n  socket  /run/user/1000/shepr/shepr.sock\n"
        );
        let older = ServerPresence::Starting(booted("0.0.9", OTHER_BUILD));
        let rendered = render(&installation, &older, None, &runtime_address());
        assert!(
            rendered.contains(
                "\nserver: starting a different build (version 0.0.9, build 5a1c09e2...), unusable by this shepr\n  run `shepr stop`, then `shepr`\n"
            ),
            "{rendered}"
        );
    }

    #[test]
    fn every_presence_is_one_plain_server_line() {
        let installation = installation(Ok(("0.1.0", THIS_BUILD)));
        for (server, line) in [
            (ServerPresence::Gone, "server: not running"),
            (
                ServerPresence::Starting(booted("0.1.0", THIS_BUILD)),
                "server: starting, pid 41233, up 3h12m, restoring panes",
            ),
            (
                ServerPresence::Running(booted("0.1.0", THIS_BUILD)),
                "server: running, pid 41233, up 3h12m",
            ),
            (
                ServerPresence::Stopping(booted("0.1.0", OTHER_BUILD)),
                "server: stopping, pid 41233",
            ),
            (
                ServerPresence::Unresponsive,
                "server: not answering; something listens at the socket but gave no status",
            ),
        ] {
            let rendered = render(&installation, &server, None, &runtime_address());
            let server_lines = rendered
                .lines()
                .filter(|text| text.starts_with("server"))
                .collect::<Vec<_>>();
            assert_eq!(server_lines, [line], "{rendered}");
        }
    }

    #[test]
    fn a_socket_override_is_named_on_the_socket_line() {
        let installation = installation(Ok(("0.1.0", THIS_BUILD)));
        let address = ServerAddress::for_runtime_dir(
            Path::new("/run/user/1000/shepr"),
            Some(Path::new("/x/a.sock")),
        )
        .expect("valid test socket path");
        let rendered = render(&installation, &ServerPresence::Gone, None, &address);
        assert!(
            rendered.ends_with("\n  socket  /x/a.sock, set by SHEPR_SOCKET_PATH\n"),
            "{rendered}"
        );
    }

    #[test]
    fn the_sibling_server_is_shown_only_when_it_differs() {
        let header =
            "shepr 0.1.0 (release, build 38df64ce2909c428)\n  /nonexistent/shepr/bin/shepr\n";
        for (sibling, expected) in [
            (Ok(("0.1.0", THIS_BUILD)), String::new()),
            (
                Ok(("0.1.0", OTHER_BUILD)),
                "shepr-server 0.1.0 (build 5a1c09e2...), a different build\n  /nonexistent/shepr/bin/shepr-server\n".to_owned(),
            ),
            (
                Ok(("0.0.9", OTHER_BUILD)),
                "shepr-server 0.0.9 (build 5a1c09e2...), a different version\n  /nonexistent/shepr/bin/shepr-server\n".to_owned(),
            ),
            (
                Err("shepr-server was not found at /nonexistent/shepr/bin/shepr-server"),
                "shepr-server unusable\n  shepr-server was not found at /nonexistent/shepr/bin/shepr-server\n".to_owned(),
            ),
        ] {
            let mut out = String::new();
            render_installation(&mut out, &installation(sibling));
            assert_eq!(out, format!("{header}{expected}"));
        }
    }

    #[test]
    fn uptime_comes_from_the_boot_clock_and_shows_two_units() {
        let boot = shepr_protocol::BootId::from_process_clock(1, Ok(Duration::from_secs(100)));
        let at = |seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        assert_eq!(uptime(&boot, at(142)), Some(Duration::from_secs(42)));
        assert_eq!(uptime(&boot, at(99)), None, "a boot in the future");
        let before_epoch =
            shepr_protocol::BootId::from_process_clock(1, Err(Duration::from_secs(5)));
        assert_eq!(uptime(&before_epoch, at(142)), None);
        for (seconds, label) in [
            (42, "42s"),
            (17 * 60 + 5, "17m"),
            (3 * 3600 + 12 * 60, "3h12m"),
            (2 * 86400 + 5 * 3600 + 59, "2d5h"),
        ] {
            assert_eq!(uptime_label(Duration::from_secs(seconds)), label);
        }
    }

    #[test]
    fn server_status_json_reports_the_running_boot() {
        let server = running_server("test", shepr_protocol::BUILD_ID);
        let value = serde_json::to_value(server_status_json(&test_paths(), &server))
            .expect("test precondition");
        assert_eq!(value["presence"], "running");
        assert_eq!(value["boot_id"], "4242-1700000000");
    }

    #[test]
    fn every_presence_is_reported_by_name() {
        let status = runtime_status("test", shepr_protocol::BUILD_ID);
        for (server, name) in [
            (ServerPresence::Gone, "gone"),
            (ServerPresence::Starting(status.clone()), "starting"),
            (ServerPresence::Running(status.clone()), "running"),
            (ServerPresence::Stopping(status), "stopping"),
            (ServerPresence::Unresponsive, "unresponsive"),
        ] {
            let value = serde_json::to_value(server_status_json(&test_paths(), &server))
                .expect("test precondition");
            assert_eq!(value["presence"], name);
        }
    }

    #[test]
    fn server_status_json_keeps_the_identity_of_starting_and_stopping_servers() {
        let other = runtime_status("0.0.0-old", "ffffffffffffffff");
        let starting = ServerPresence::Starting(other.clone());
        let json = server_status_json(&test_paths(), &starting);
        assert_eq!(json.presence(), ServerPresenceJson::Starting);
        assert_eq!(
            json.identity().map(|identity| identity.boot_id.as_str()),
            Some("4242-1700000000")
        );
        assert_eq!(build_compatible(&starting), Some(false));

        let stopping = ServerPresence::Stopping(other);
        let json = server_status_json(&test_paths(), &stopping);
        assert_eq!(json.presence(), ServerPresenceJson::Stopping);
    }

    #[test]
    fn an_unresponsive_server_has_no_identity() {
        let server = ServerPresence::Unresponsive;
        let json = server_status_json(&test_paths(), &server);
        assert_eq!(json.presence(), ServerPresenceJson::Unresponsive);
        assert_eq!(json.identity(), None);
        assert_eq!(build_compatible(&server), None);
    }

    /// What a machine's own `shepr` reported: `installed` is its build, the
    /// server is `server` (or none), with `summary` counts.
    fn remote_overview(
        installed: &str,
        server: Option<(&str, &str)>,
        summary: Option<ServerSummaryJson>,
    ) -> StatusOverviewJson {
        use shepr_api::schema::{ServerIdentity, ServerStatus};
        StatusOverviewJson {
            local_client: ClientStatusJson {
                identity: Some(shepr_protocol::BuildVersion {
                    version: "0.1.0".into(),
                    build_id: installed.parse().expect("build identity"),
                }),
                binary: None,
                server: None,
            },
            server: ServerStatusJson {
                state: server.map_or(ServerStatus::Gone, |(presence, build)| {
                    let identity = ServerIdentity {
                        version: "0.1.0".into(),
                        build_id: build.parse().expect("build identity"),
                        boot_id: shepr_protocol::BootId::from_process_clock(
                            41233,
                            Ok(Duration::from_secs(BOOTED_AT)),
                        ),
                    };
                    match presence {
                        "starting" => ServerStatus::Starting(identity),
                        "stopping" => ServerStatus::Stopping(identity),
                        _ => ServerStatus::Running(identity),
                    }
                }),
                socket: "/run/user/1000/shepr/shepr.sock".into(),
            },
            summary,
        }
    }

    #[test]
    fn a_machine_line_judges_the_remote_server_against_this_build() {
        let this = shepr_protocol::BUILD_ID;
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(BOOTED_AT + 3 * 3600 + 12 * 60);
        let counts = ServerSummaryJson {
            workspaces: 3,
            panes: 7,
            agents: 4,
            blocked_agents: 1,
        };
        assert_eq!(
            machine_line(
                &remote_overview(this, Some(("running", this)), Some(counts)),
                now
            ),
            "running, pid 41233, up 3h12m   workspaces 3   panes 7   agents 4 (1 blocked)"
        );
        assert_eq!(
            machine_line(&remote_overview(this, None, None), now),
            "not running"
        );
        // A server older than the install beside it.
        assert_eq!(
            machine_line(
                &remote_overview(this, Some(("running", OTHER_BUILD)), None),
                now
            ),
            "running, pid 41233, up 3h12m; the server is not the installed build (server 5a1c09e2..., installed 38df64ce...): restart it to update"
                .replace("38df64ce...", &short_build(this.parse().expect("build")))
        );
        // A host whose install and server agree, but on another build.
        assert_eq!(
            machine_line(&remote_overview(OTHER_BUILD, None, None), now),
            "not running; installed build 5a1c09e2... is not this shepr's"
        );
        assert_eq!(
            machine_line(&remote_overview(this, Some(("stopping", this)), None), now),
            "stopping, pid 41233"
        );
    }

    #[test]
    fn the_machines_section_lists_answers_and_failures_and_names_an_empty_fleet() {
        let paths = test_paths();
        let machine = |label: &str| shepr_config::MachineConfig {
            label: shepr_config::MachineLabel::parse(label).expect("label"),
            ssh: shepr_config::SshTarget::parse(label).expect("target"),
            palette: shepr_config::DEFAULT_LOCAL_HUE,
        };
        let (dm6, speilegg) = (machine("dm6"), machine("speilegg"));
        let rendered = render_machines(
            &[
                (
                    &dm6,
                    Ok(MachineStatus {
                        executable: "/nonexistent/remote/bin/shepr".into(),
                        overview: remote_overview(shepr_protocol::BUILD_ID, None, None),
                    }),
                ),
                (
                    &speilegg,
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "connect timed out",
                    )),
                ),
            ],
            &paths,
            SystemTime::UNIX_EPOCH,
        );
        assert_eq!(
            rendered,
            "\nmachines\n  dm6       not running\n  speilegg  unreachable: connect timed out\n"
        );
        let empty = render_machines(&[], &paths, SystemTime::UNIX_EPOCH);
        assert!(
            empty.starts_with("\nmachines: none configured in "),
            "{empty}"
        );
    }

    #[test]
    fn build_compatibility_compares_the_build_not_the_version() {
        assert_eq!(
            build_compatible(&running_server("0.0.0-old", shepr_protocol::BUILD_ID)),
            Some(true)
        );
        assert_eq!(
            build_compatible(&running_server(
                shepr_protocol::build_version().as_str(),
                "ffffffffffffffff"
            )),
            Some(false)
        );
    }

    #[test]
    fn all_is_rejected_with_scoped_status_subcommands() {
        for args in [
            ["shepr", "status", "--all", "server"],
            ["shepr", "status", "--all", "client"],
        ] {
            let matches = super::super::spec::command()
                .try_get_matches_from(args)
                .expect("clap hands this combination to the typed status parser");
            let (_, status_matches) = matches.subcommand().expect("status subcommand");
            assert!(parse(status_matches).is_none(), "{args:?}");
        }
    }

    #[test]
    fn root_json_remains_available_with_a_scoped_status_command() {
        let matches = super::super::spec::command()
            .try_get_matches_from(["shepr", "status", "--json", "server"])
            .expect("root JSON may scope status to the server");
        let (_, status_matches) = matches.subcommand().expect("status subcommand");

        assert!(matches!(
            parse(status_matches),
            Some(ParsedCommand::Local(Command::Server { json: true }))
        ));
    }

    #[test]
    fn all_with_a_scoped_status_command_is_a_usage_error() {
        for args in [
            ["shepr", "status", "--all", "server"],
            ["shepr", "status", "--all", "client"],
        ] {
            let argv = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
            assert!(
                matches!(crate::cli::parse_launch(&argv), Err(2)),
                "{args:?}"
            );
        }
    }
}
