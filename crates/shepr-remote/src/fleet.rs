//! The CLI's `--all`: every configured machine's status, and the stop of its
//! server, from one host.
//!
//! The TUI's discovery accepts only a `shepr` of this build, since it has to
//! serve the machine. These commands talk to whichever `shepr` is installed
//! there instead, through the cross-build surface that host's `shepr` keeps:
//! `status --json` and the conditional `stop --expect-boot`. A host left on
//! an older build is what a fleet stop is most often for.
//!
//! Every SSH command is BatchMode on shepr's own control socket, so nothing
//! prompts; a machine that needs a login reports so and is not waited on.

use std::io;
use std::time::Instant;

use shepr_api::schema::{ServerStatus, StatusOverviewJson};
use shepr_launch::restart::StopOutcome;

use crate::args::RemoteCliCommand;
use crate::discovery::{candidate_command, installed_remote_shepr_candidates, last_json_record};
use crate::failure::{RemoteExit, SshExit, failure_evidence, remote_compatibility_error};
use crate::limits::FLEET_STATUS_BUDGET;
use crate::machine::{MachineConfig, RemoteExecutable};
use crate::server_lifecycle::stop_remote_server_with_ssh;
use crate::ssh::{RemoteSsh, command_failed};

/// What a machine's own `shepr` reported, and which `shepr` that was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineStatus {
    /// The path of the `shepr` that answered.
    pub executable: String,
    pub overview: StatusOverviewJson,
}

/// How a machine's server came out of a fleet stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineStop {
    /// The server that was running there stopped.
    Stopped,
    /// No server was running.
    NotRunning,
    /// Another server replaced the observed one while it was stopped, and was
    /// left running.
    Replaced,
}

/// Reads `machine`'s status through its own `shepr`, whatever its build.
pub fn machine_status(
    paths: &shepr_paths::AppPaths,
    machine: &MachineConfig,
) -> io::Result<MachineStatus> {
    let ssh = fleet_ssh(paths, machine)?;
    read_status(&ssh).map(|(executable, overview)| MachineStatus {
        executable: executable.shell_word().to_owned(),
        overview,
    })
}

/// Stops `machine`'s server, whatever its build, naming the boot its status
/// reported so a server that replaced it is left running.
pub fn stop_machine(
    paths: &shepr_paths::AppPaths,
    machine: &MachineConfig,
) -> io::Result<MachineStop> {
    let ssh = fleet_ssh(paths, machine)?;
    let (executable, overview) = read_status(&ssh)?;
    let boot_id = match stop_plan(&overview.server.state)? {
        StopPlan::Nothing => return Ok(MachineStop::NotRunning),
        StopPlan::Stop(boot_id) => boot_id,
    };
    Ok(
        match stop_remote_server_with_ssh(&ssh, &executable, &boot_id)? {
            StopOutcome::Stopped => MachineStop::Stopped,
            StopOutcome::NoServer => MachineStop::NotRunning,
            StopOutcome::BootChanged => MachineStop::Replaced,
        },
    )
}

/// Runs `operation` for every machine at once and returns the results in
/// configuration order.
pub fn on_every_machine<T: Send>(
    machines: &[MachineConfig],
    operation: impl Fn(&MachineConfig) -> T + Sync,
) -> Vec<T> {
    std::thread::scope(|scope| {
        let operation = &operation;
        let handles: Vec<_> = machines
            .iter()
            .map(|machine| scope.spawn(move || operation(machine)))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
            })
            .collect()
    })
}

fn fleet_ssh(paths: &shepr_paths::AppPaths, machine: &MachineConfig) -> io::Result<RemoteSsh> {
    // The deadline bounds real ssh IO for this machine.
    let deadline = Instant::now() + FLEET_STATUS_BUDGET; // clock-io-ok: bounds real ssh IO
    RemoteSsh::new(machine.ssh.clone(), paths, deadline)
}

/// The first installed `shepr` that reports a status, and what it reported.
fn read_status(ssh: &RemoteSsh) -> io::Result<(RemoteExecutable, StatusOverviewJson)> {
    let candidates = installed_remote_shepr_candidates(ssh)?;
    first_answering(candidates, ssh.target(), |candidate| {
        overview_of(ssh, candidate)
    })
}

/// Asks each candidate in turn. A candidate that ran and failed, or is gone,
/// gives way to the next; a failure that learned nothing about the host (the
/// link, a host key) ends the search. With no candidate answering, the first
/// candidate's failure is the reason, or, with none at all, that no `shepr` is
/// installed.
fn first_answering<T>(
    candidates: Vec<RemoteExecutable>,
    target: &impl std::fmt::Display,
    mut ask: impl FnMut(&RemoteExecutable) -> io::Result<T>,
) -> io::Result<(RemoteExecutable, T)> {
    let mut first_rejection = None;
    for candidate in candidates {
        match ask(&candidate) {
            Ok(answer) => return Ok((candidate, answer)),
            Err(error) if failure_evidence(&error).rejects_candidate() => {
                first_rejection.get_or_insert(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(first_rejection.unwrap_or_else(|| {
        remote_compatibility_error(format!("no shepr is installed on {target}"))
    }))
}

fn overview_of(ssh: &RemoteSsh, candidate: &RemoteExecutable) -> io::Result<StatusOverviewJson> {
    let command = candidate_command(
        candidate,
        &candidate.command(&RemoteCliCommand::Overview.args()),
    );
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        let context = if SshExit::from_code(output.status.code())
            == SshExit::Remote(RemoteExit::CandidateMissing)
        {
            "the remote shepr is gone"
        } else {
            "remote shepr status failed"
        };
        return Err(command_failed(
            context,
            &output,
            ssh.has_established_session(),
        ));
    }
    parse_overview(&String::from_utf8_lossy(&output.stdout))
}

/// The last line of `stdout` that is a status overview: a login banner or
/// other noise before it is skipped.
fn parse_overview(stdout: &str) -> io::Result<StatusOverviewJson> {
    last_json_record(stdout)
        .ok_or_else(|| remote_compatibility_error("the remote shepr reported no status JSON"))
}

#[derive(Debug, PartialEq, Eq)]
enum StopPlan {
    Nothing,
    /// Stop the server of this boot. A stopping server is stopped by its boot
    /// too, so the stop waits for it to go.
    Stop(shepr_protocol::BootId),
}

/// Unlike a Restart (`stop_server_of_another_build`, which treats a stopping
/// server as none because its starting bridge waits the shutdown out), a fleet
/// stop has nothing after it to wait, so it stops a stopping server by its
/// boot and lets the remote command wait for that boot to go.
fn stop_plan(state: &ServerStatus) -> io::Result<StopPlan> {
    match state {
        ServerStatus::Gone => Ok(StopPlan::Nothing),
        ServerStatus::Starting(identity)
        | ServerStatus::Running(identity)
        | ServerStatus::Stopping(identity) => Ok(StopPlan::Stop(identity.boot_id.clone())),
        ServerStatus::Unresponsive => Err(io::Error::other(
            shepr_launch::EndpointFailure::remote_repair(
                "the server there is not answering, so it cannot be stopped by its boot; stop it on that host",
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &str) -> RemoteExecutable {
        RemoteExecutable::parse(path).expect("test path")
    }

    #[test]
    fn the_first_candidate_that_answers_is_used_and_rejected_ones_give_way() {
        let candidates = vec![executable("/a/shepr"), executable("/b/shepr")];
        let (chosen, answer) = first_answering(candidates, &"host", |candidate| {
            if candidate.shell_word() == "/a/shepr" {
                Err(remote_compatibility_error("an old shepr"))
            } else {
                Ok(7)
            }
        })
        .expect("the second answers");
        assert_eq!(chosen.shell_word(), "/b/shepr");
        assert_eq!(answer, 7);
    }

    #[test]
    fn a_link_failure_ends_the_search_and_no_candidate_names_the_host() {
        let mut asked = 0;
        let error = first_answering(
            vec![executable("/a/shepr"), executable("/b/shepr")],
            &"host",
            |_| -> io::Result<()> {
                asked += 1;
                Err(io::Error::from(io::ErrorKind::TimedOut))
            },
        )
        .expect_err("the link failed");
        assert_eq!(asked, 1);
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        let error = first_answering(Vec::new(), &"build", |_| -> io::Result<()> { Ok(()) })
            .expect_err("nothing installed");
        assert!(error.to_string().contains("no shepr is installed on build"));
    }

    #[test]
    fn the_overview_is_the_last_status_line_after_any_banner() {
        let line = r#"{"local_client":{"version":"1.0","build_id":"0123456789abcdef","binary":null,"server":null},"server":{"presence":"gone","version":null,"build_id":null,"boot_id":null,"socket":"s"}}"#;
        let overview =
            parse_overview(&format!("Welcome to build\n\n{line}\n")).expect("the status line");
        assert_eq!(overview.server.state, ServerStatus::Gone);
        assert!(parse_overview("Welcome\n").is_err());
    }

    #[test]
    fn a_stop_names_the_boot_of_any_answering_server_and_skips_a_gone_one() {
        let identity = shepr_api::schema::ServerIdentity {
            version: "1.0".into(),
            build_id: "0123456789abcdef".parse().expect("build"),
            boot_id: "17-23".parse().expect("boot"),
        };
        assert_eq!(
            stop_plan(&ServerStatus::Gone).expect("plan"),
            StopPlan::Nothing
        );
        for state in [
            ServerStatus::Starting(identity.clone()),
            ServerStatus::Running(identity.clone()),
            ServerStatus::Stopping(identity.clone()),
        ] {
            assert_eq!(
                stop_plan(&state).expect("plan"),
                StopPlan::Stop(identity.boot_id.clone())
            );
        }
        let error = stop_plan(&ServerStatus::Unresponsive).expect_err("unresponsive server");
        assert_eq!(
            shepr_launch::EndpointFailure::from_error(&error).cause(),
            shepr_launch::FailureCause::RemoteRepair
        );
    }
}
