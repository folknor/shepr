//! The startup step before the TUI takes the terminal: machines that need an
//! SSH prompt get it here, one at a time, because the TUI connects with
//! `BatchMode=yes` and cannot answer one. The mechanism lives in
//! `shepr_remote::preflight`; this module supplies the config, the terminal
//! and the words.

use std::io::IsTerminal;

use shepr_config::MachineConfig;
use shepr_remote::{MachineCheck, PreflightOutcome};

/// Checks every configured machine and authenticates the ones that need it.
/// Unreachable machines are not an error here: the client shows them offline
/// and keeps retrying. Host keys are never accepted; a machine whose key is
/// unknown or changed is named so the operator can fix it.
pub(crate) fn authenticate_machines(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
) {
    let machines = config.machines();
    if machines.is_empty() {
        return;
    }
    let can_prompt = std::io::stdin().is_terminal();
    let settings = shepr_remote::SavedSshSettings {
        manage_ssh_config: config.remote().manage_ssh_config,
    };
    let ssh = shepr_remote::SavedSshPreflight::new(paths, settings);
    let outcomes = shepr_remote::preflight(machines, &ssh, can_prompt, |machine| {
        crate::cli::print_notice(&prompt_notice(machine));
    });
    for notice in result_notices(machines, &outcomes, can_prompt) {
        crate::cli::print_notice(&notice);
    }
}

/// Printed just before the interactive ssh for one machine, so its prompt is
/// attributable.
fn prompt_notice(machine: &MachineConfig) -> String {
    format!(
        "shepr: machine {} ({}) needs authentication; running ssh for it.",
        machine.label,
        machine.ssh.as_str()
    )
}

/// What is worth telling the operator after the preflight. Offline and
/// incompatible machines are left to the client, which shows their state and
/// retries; only what the operator has to act on is printed.
fn result_notices(
    machines: &[MachineConfig],
    outcomes: &[PreflightOutcome],
    can_prompt: bool,
) -> Vec<String> {
    let mut notices = Vec::new();
    for (machine, outcome) in machines.iter().zip(outcomes) {
        match (&outcome.check, &outcome.authentication) {
            (_, Some(Err(error))) => notices.push(format!(
                "shepr: authentication for machine {} failed: {error}. The client keeps retrying it.",
                outcome.label
            )),
            (MachineCheck::NeedsAuthentication(_), None) if !can_prompt => {
                notices.push(format!(
                    "shepr: machine {} needs authentication, but there is no terminal to prompt on; run shepr from an interactive terminal.",
                    outcome.label
                ));
            }
            (MachineCheck::HostKey(diagnostic), _) => {
                let mut notice = format!("shepr: machine {}: {diagnostic}", outcome.label);
                for hint in shepr_remote::saved_ssh_error_hint(diagnostic, machine.ssh.as_str()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_remote::machine::MachineLabel;

    fn machine(label: &str) -> MachineConfig {
        MachineConfig {
            label: MachineLabel::parse(label).expect("test precondition"),
            ssh: shepr_config::SshTarget::parse(format!("{label}.example"))
                .expect("test precondition"),
        }
    }

    fn outcome(
        machine: &MachineConfig,
        check: MachineCheck,
        authentication: Option<Result<(), String>>,
    ) -> PreflightOutcome {
        PreflightOutcome {
            label: machine.label.clone(),
            check,
            authentication,
        }
    }

    fn diagnostic(message: &str) -> shepr_remote::SshFailureDiagnostic {
        shepr_remote::SshFailureDiagnostic::from_ssh_output(Some(255), message.into())
    }

    #[test]
    fn the_prompt_notice_names_the_machine_and_its_target() {
        let notice = prompt_notice(&machine("build"));
        assert!(notice.contains("build"), "{notice}");
        assert!(notice.contains("build.example"), "{notice}");
    }

    #[test]
    fn only_what_the_operator_can_act_on_is_reported() {
        let machines = [
            machine("ready"),
            machine("offline"),
            machine("authenticated"),
            machine("refused"),
            machine("hostkey"),
            machine("incompatible"),
        ];
        let outcomes = [
            outcome(&machines[0], MachineCheck::Ready, None),
            outcome(
                &machines[1],
                MachineCheck::Offline(diagnostic("Connection refused")),
                None,
            ),
            outcome(
                &machines[2],
                MachineCheck::NeedsAuthentication(diagnostic("Permission denied (publickey)")),
                Some(Ok(())),
            ),
            outcome(
                &machines[3],
                MachineCheck::NeedsAuthentication(diagnostic("Permission denied (publickey)")),
                Some(Err("ssh exited with exit status: 255".into())),
            ),
            outcome(
                &machines[4],
                MachineCheck::HostKey(diagnostic("Host key verification failed.")),
                None,
            ),
            outcome(
                &machines[5],
                MachineCheck::Incompatible(diagnostic("another build")),
                None,
            ),
        ];
        let notices = result_notices(&machines, &outcomes, true);
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices[0].contains("refused") && notices[0].contains("failed"));
        assert!(notices[1].contains("hostkey") && notices[1].contains("known_hosts"));
    }

    #[test]
    fn a_machine_needing_authentication_without_a_terminal_is_named() {
        let machines = [machine("build")];
        let outcomes = [outcome(
            &machines[0],
            MachineCheck::NeedsAuthentication(diagnostic("Permission denied (publickey)")),
            None,
        )];
        assert!(result_notices(&machines, &outcomes, true).is_empty());
        let notices = result_notices(&machines, &outcomes, false);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("no terminal"), "{notices:?}");
    }
}
