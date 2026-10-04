//! The operator wording of the startup step: the restart offers, the notices
//! printed after it and the hints for a failed machine check. It holds no
//! behaviour beyond building the text; the engine that asks, stops and
//! authenticates is in the parent module.

use shepr_config::{MachineConfig, SshTarget};
use shepr_launch::restart::RestartResult;
use shepr_launch::status::RuntimeStatus;
use shepr_launch::{EndpointFailure, FailureCause, SshFailureClass};
use shepr_remote::{DifferentBuildServer, MachineCheck, PreflightOutcome};

pub(super) fn local_offer(status: &RuntimeStatus) -> String {
    format!(
        "shepr: the local shepr server is a different build (server build {}, boot {}, this shepr build {}).\n\
         Restarting it stops that server, which ends every pane process it hosts.\n\
         The saved layout is restored with fresh shells, and agents are resumed where they can be.\n\
         Restart it now? [y/N] ",
        status.build_id,
        status.boot_id,
        shepr_protocol::BUILD_ID
    )
}

pub(super) fn remote_offer(machine: &MachineConfig, server: &DifferentBuildServer) -> String {
    format!(
        "shepr: the shepr server on machine {} ({}) is a different build (server build {}, boot {}, this shepr build {}).\n\
         Restarting it stops that server, which ends every pane process it hosts on that machine. The saved layout is restored with fresh shells, and agents are resumed where they can be.\n\
         Restart it now? [y/N] ",
        machine.label,
        machine.ssh,
        server.build_id,
        server.boot_id,
        shepr_protocol::BUILD_ID
    )
}

/// What the operator is told about the local server's restart. A server that
/// was kept running, or that no one could be asked about, is reported by the
/// launch that follows, with the stop command.
pub(super) fn local_notice(local: &RestartResult) -> Option<String> {
    match local {
        RestartResult::NotNeeded | RestartResult::NoTerminal | RestartResult::Declined => None,
        RestartResult::Stopped => Some(
            "shepr: stopped the local server of a different build; one of this build starts now."
                .to_owned(),
        ),
        RestartResult::NoServer => Some(
            "shepr: the local server of a different build had already stopped; one of this build starts now."
                .to_owned(),
        ),
        RestartResult::OccupantChanged => Some(
            "shepr: the local server changed while it was being stopped; no stop was sent to a new occupant."
                .to_owned(),
        ),
        RestartResult::Failed(error) => {
            Some(format!("shepr: could not stop the local server: {error}"))
        }
    }
}

/// Printed just before the interactive ssh for one machine, so its prompt is
/// attributable.
pub(super) fn prompt_notice(machine: &MachineConfig) -> String {
    format!(
        "shepr: machine {} ({}) needs authentication; running ssh for it.",
        machine.label, machine.ssh
    )
}

/// The command an operator runs to stop a remote server themselves.
pub(super) fn remote_stop_command(
    machine: &MachineConfig,
    server: &DifferentBuildServer,
) -> String {
    let arguments = shepr_remote::RemoteCliCommand::ServerStop {
        expected_boot: &server.boot_id,
    }
    .args()
    .into_iter()
    .map(shepr_remote::shell_quote)
    .collect::<Vec<_>>()
    .join(" ");
    format!(
        "ssh {} {} {arguments}",
        machine.ssh.shell_word(),
        server.executable.shell_word(),
    )
}

/// What is worth telling the operator after the preflight. Offline machines are
/// left to the client, which shows their state and retries; everything the
/// operator can act on is printed, including a machine that cannot be served
/// and a server of another build that was left running.
pub(super) fn result_notices(outcomes: &[PreflightOutcome], can_prompt: bool) -> Vec<String> {
    let mut notices = Vec::new();
    for outcome in outcomes {
        let machine = &outcome.machine;
        if let Some(Err(error)) = &outcome.authentication {
            notices.push(format!(
                "shepr: authentication for machine {} failed: {error}. The client keeps retrying it.",
                machine.label
            ));
        }
        notices.extend(restart_notice(outcome));
        match (&outcome.check, &outcome.authentication) {
            (MachineCheck::NeedsAuthentication(_), None) if !can_prompt => {
                notices.push(format!(
                    "shepr: machine {} needs authentication, but there is no terminal to prompt on; run shepr from an interactive terminal.",
                    machine.label
                ));
            }
            (MachineCheck::NeedsAuthentication(failure), Some(Ok(()))) => {
                notices.push(format!(
                    "shepr: machine {} still refuses the client's connection after ssh authenticated: {failure}. The client keeps retrying it.",
                    machine.label
                ));
            }
            (MachineCheck::HostKey(failure), _) => {
                let mut notice = format!("shepr: machine {}: {failure}", machine.label);
                for hint in machine_failure_hints(failure, &machine.ssh) {
                    notice.push('\n');
                    notice.push_str(&hint);
                }
                notices.push(notice);
            }
            (MachineCheck::Incompatible(failure), _) => notices.push(format!(
                "shepr: machine {} cannot be used: {failure}. {}",
                machine.label,
                failure.disposition().client_action()
            )),
            (MachineCheck::Failed(failure), _) => {
                let client_action = failure.disposition().client_action();
                let mut notice = format!(
                    "shepr: machine {} could not be checked: {failure}. {client_action}",
                    machine.label
                );
                for hint in machine_failure_hints(failure, &machine.ssh) {
                    notice.push('\n');
                    notice.push_str(&hint);
                }
                notices.push(notice);
            }
            _ => {}
        }
    }
    notices
}

/// Operator hint lines for a configured machine's failure, derived from its
/// typed cause, with the configured target as what they name. Empty when the
/// cause has no hint.
pub(super) fn machine_failure_hints(failure: &EndpointFailure, target: &SshTarget) -> Vec<String> {
    match failure.cause() {
        FailureCause::Ssh(SshFailureClass::HostKey) => vec![
            "hint: configured machines use strict host-key checking; add the host key to the configured known_hosts file, then retry."
                .to_owned(),
        ],
        FailureCause::Ssh(SshFailureClass::Configuration) => vec![
            "hint: check the configured SSH target and local SSH configuration; OpenSSH reports the file and line for configuration errors."
                .to_owned(),
        ],
        FailureCause::Ssh(SshFailureClass::Authentication) => vec![
            format!(
                "hint: verify SSH access first with `{}`.",
                shepr_remote::ssh_check_command(target)
            ),
            "hint: if your SSH key has a passphrase, load it into ssh-agent with `ssh-add` before retrying."
                .to_owned(),
        ],
        _ => Vec::new(),
    }
}

/// What came of the restart offer for one machine, with what to do when its
/// server of another build was left running.
pub(super) fn restart_notice(outcome: &PreflightOutcome) -> Option<String> {
    let restart = outcome.restart.as_ref()?;
    let machine = &outcome.machine;
    let label = &machine.label;
    let server = match &outcome.check {
        MachineCheck::DifferentBuild(server) => Some(server),
        _ => None,
    };
    let left_running = || {
        server.map(|server| format!(
            "the shepr server on machine {label} is a different build (build {}, this shepr is build {}) and is left running, so the machine is unavailable. To restart it, run `{}` (this ends its pane processes; the layout is restored when a server starts again), then run shepr again.",
            server.build_id,
            shepr_protocol::BUILD_ID,
            remote_stop_command(machine, server)
        ))
    };
    let notice = match restart {
        RestartResult::NotNeeded => return None,
        RestartResult::NoTerminal => format!(
            "{} Run shepr from an interactive terminal to be offered a restart.",
            left_running()?
        ),
        RestartResult::Declined => left_running()?,
        RestartResult::Failed(error) => format!(
            "could not stop the shepr server on machine {label}: {error}\n{}",
            left_running()?
        ),
        RestartResult::Stopped => format!(
            "stopped the shepr server of a different build on machine {label}; one of this build starts when the client attaches."
        ),
        RestartResult::NoServer => format!(
            "the shepr server of a different build on machine {label} had already stopped; one of this build starts when the client attaches."
        ),
        RestartResult::OccupantChanged => match &outcome.check {
            MachineCheck::DifferentBuild(server) => format!(
                "the shepr server on machine {label} was replaced while it was being stopped; boot {} now answers as another different build, and no stop was sent to it. Run shepr again to be offered a restart.",
                server.boot_id
            ),
            _ => format!(
                "the shepr server on machine {label} changed while it was being stopped; no stop was sent to a replacement."
            ),
        },
    };
    Some(format!("shepr: {notice}"))
}
