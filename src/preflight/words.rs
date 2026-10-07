//! Adapts configured machines and remote preflight outcomes to launch-owned
//! operator guidance. Selection stays here because shepr-launch must not
//! depend on shepr-remote or the client configuration layer.

use shepr_config::{MachineConfig, SshTarget};
use shepr_launch::EndpointFailure;
use shepr_launch::guidance::{MachinePreflightNotice, machine_preflight_notice};
use shepr_launch::restart::RestartResult;
use shepr_launch::status::RuntimeStatus;
use shepr_remote::{MachineCheck, PreflightOutcome};

pub(super) fn local_offer(status: &RuntimeStatus) -> String {
    shepr_launch::guidance::local_offer(status)
}

pub(super) fn local_notice(local: &RestartResult) -> Option<String> {
    shepr_launch::guidance::local_notice(local)
}

/// Printed just before the interactive ssh for one machine, so its prompt is
/// attributable.
pub(super) fn prompt_notice(machine: &MachineConfig) -> String {
    shepr_launch::guidance::ssh_prompt_notice(&machine.label, &machine.ssh)
}

/// What is worth telling the operator after the preflight. Offline machines are
/// left to the client, which shows their state and retries; everything the
/// operator can act on is printed, including a machine that cannot be served.
pub(super) fn result_notices(outcomes: &[PreflightOutcome], can_prompt: bool) -> Vec<String> {
    let mut notices = Vec::new();
    for outcome in outcomes {
        let machine = &outcome.machine;
        if let Some(Err(error)) = &outcome.authentication {
            // A prompt runs only after a check that needs authentication, and
            // a failed prompt keeps that check: its disposition says whether
            // the client retries the machine by itself.
            let disposition = match &outcome.check {
                MachineCheck::NeedsAuthentication(failure) => failure.disposition(),
                _ => shepr_launch::FailureDisposition::Authentication,
            };
            notices.push(machine_preflight_notice(
                &machine.label,
                MachinePreflightNotice::AuthenticationFailed(error, disposition),
            ));
        }
        match (&outcome.check, &outcome.authentication) {
            (MachineCheck::NeedsAuthentication(_), None) if !can_prompt => {
                notices.push(machine_preflight_notice(
                    &machine.label,
                    MachinePreflightNotice::NoTerminal,
                ));
            }
            (MachineCheck::NeedsAuthentication(failure), Some(Ok(()))) => {
                notices.push(machine_preflight_notice(
                    &machine.label,
                    MachinePreflightNotice::AuthenticationRefused(failure),
                ));
            }
            (MachineCheck::HostKey(failure), _) => {
                let mut notice = machine_preflight_notice(
                    &machine.label,
                    MachinePreflightNotice::HostKey(failure),
                );
                for hint in machine_failure_hints(failure, &machine.ssh) {
                    notice.push('\n');
                    notice.push_str(&hint);
                }
                notices.push(notice);
            }
            (MachineCheck::Incompatible(failure), _) => notices.push(machine_preflight_notice(
                &machine.label,
                MachinePreflightNotice::Incompatible(failure),
            )),
            (MachineCheck::Failed(failure), _) => {
                let mut notice = machine_preflight_notice(
                    &machine.label,
                    MachinePreflightNotice::Failed(failure),
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
    shepr_launch::guidance::machine_failure_hints(failure, &shepr_remote::ssh_check_command(target))
}
