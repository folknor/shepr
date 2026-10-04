//! The operator wording of the startup step: the local restart offer, the
//! notices printed after it and the hints for a failed machine check. It holds
//! no behaviour beyond building the text; the engine that asks, stops and
//! authenticates is in the parent module.

use shepr_config::{MachineConfig, SshTarget};
use shepr_launch::restart::RestartResult;
use shepr_launch::status::RuntimeStatus;
use shepr_launch::{EndpointFailure, FailureCause, SshFailureClass};
use shepr_remote::{MachineCheck, PreflightOutcome};

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

/// What is worth telling the operator after the preflight. Offline machines are
/// left to the client, which shows their state and retries; everything the
/// operator can act on is printed, including a machine that cannot be served.
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
