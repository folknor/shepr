//! The startup step before the TUI takes the terminal. The TUI connects with
//! `BatchMode=yes`, so two things that need the operator happen here first:
//!
//! - machines that need an SSH prompt get it, one at a time, and are checked
//!   again afterwards;
//! - a running server of another build, local or remote, is offered a restart.
//!   Every offer comes after the last authentication prompt, so prompts and
//!   questions never interleave.
//!
//! The mechanism lives in `shepr_remote::preflight`; this module supplies the
//! config, the terminal, the local server and the words.

use std::io::{self, IsTerminal, Write as _};
use std::os::fd::AsRawFd as _;

use shepr_api::RuntimeStatus;
use shepr_api::server_stop::ServerStopError;
use shepr_config::MachineConfig;
use shepr_remote::{DifferentBuildServer, MachineCheck, PreflightOutcome, RestartResult};

use crate::limits::MAX_LOCAL_OFFERS;

/// Authenticates the machines that need it, then offers to restart each running
/// server of another build: the local one first, then the machines'. Every
/// question needs a terminal; without one nothing is asked and nothing is
/// stopped.
///
/// Unreachable machines are not an error here: the client shows them offline
/// and keeps retrying. Host keys are never accepted; a machine whose key is
/// unknown or changed is named so the operator can fix it. A server left
/// running is reported with what to do about it.
pub(crate) fn run(config: &shepr_config::ValidatedClientConfig, paths: &shepr_config::AppPaths) {
    // Questions go to stderr and answers come from stdin, and ssh prompts use
    // the terminal too, so asking needs both to be a terminal.
    let can_prompt = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let machines = config.machines();
    let ssh = shepr_remote::MachineSshPreflight::new(paths);
    let mut outcomes = if machines.is_empty() {
        Vec::new()
    } else {
        shepr_remote::preflight(machines, &ssh, can_prompt, |machine| {
            crate::cli::print_notice(&prompt_notice(machine));
        })
    };

    let local = restart_local(
        can_prompt,
        || local_server_status(paths),
        |boot_id| shepr_api::server_stop::stop_active_server(paths, Some(boot_id)),
        |status| {
            let consent = confirm(&local_offer(status));
            if consent {
                crate::cli::print_notice(&"shepr: stopping the local server.");
            }
            consent
        },
    );
    shepr_remote::restart_different_builds(
        machines,
        &mut outcomes,
        &ssh,
        can_prompt,
        |machine, server| {
            if confirm(&remote_offer(machine, server)) {
                crate::cli::print_notice(&format!(
                    "shepr: stopping the server on machine {}.",
                    machine.label
                ));
                shepr_remote::RestartDecision::Restart
            } else {
                shepr_remote::RestartDecision::Keep
            }
        },
    );

    for notice in local_notice(&local)
        .into_iter()
        .chain(result_notices(machines, &outcomes, can_prompt))
    {
        crate::cli::print_notice(&notice);
    }
}

/// The status of a running local server of any build, when this client can
/// start its replacement. A socket override names an existing server but is
/// not an address this client can launch for, so it gets no restart offer. A
/// server that cannot be read is also left for the launch that follows to
/// report.
fn local_server_status(paths: &shepr_config::AppPaths) -> Option<RuntimeStatus> {
    if !paths.server_address().is_runtime_address() {
        return None;
    }
    match shepr_remote::local_server::running_server_status(paths) {
        Ok(status) => status,
        Err(error) => {
            tracing::warn!(%error, "cannot read the local server for a restart offer");
            None
        }
    }
}

/// Asks `question` on stderr and reads one line directly from stdin's file
/// descriptor. Single-byte reads leave any type-ahead after the answer for the
/// client's raw-fd input loop. Only an explicit yes counts: an empty answer,
/// anything else and a closed stdin all keep the server.
fn confirm(question: &str) -> bool {
    eprint!("{question}");
    if std::io::stderr().flush().is_err() {
        return false;
    }
    let stdin = std::io::stdin();
    let stdin_fd = stdin.as_raw_fd();
    match read_answer_line(|byte| shepr_platform::read_fd(stdin_fd, byte)) {
        Ok(Some(answer)) => is_yes(&answer),
        Ok(None) | Err(_) => false,
    }
}

fn read_answer_line(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
) -> io::Result<Option<String>> {
    let mut answer = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let count = match read(&mut byte) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            if answer.is_empty() {
                return Ok(None);
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        answer.push(byte[0]);
    }
    String::from_utf8(answer)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn local_offer(status: &RuntimeStatus) -> String {
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

fn remote_offer(machine: &MachineConfig, server: &DifferentBuildServer) -> String {
    format!(
        "shepr: the shepr server on machine {} ({}) is a different build (server build {}, boot {}, this shepr build {}).\n\
         Restarting it stops that server, which ends every pane process it hosts on that machine. The saved layout is restored with fresh shells, and agents are resumed where they can be.\n\
         Restart it now? [y/N] ",
        machine.label,
        machine.ssh.as_str(),
        server.build_id,
        server.boot_id,
        shepr_protocol::BUILD_ID
    )
}

/// How the offer to restart the local server ended.
#[derive(Debug, PartialEq, Eq)]
enum LocalRestart {
    /// No server of another build is running.
    NotNeeded,
    /// There was no terminal to ask on, so the server was left running.
    NoTerminal,
    /// The operator kept the server running.
    Declined,
    /// The server was stopped; the launch that follows starts one of this build.
    Stopped,
    /// The named server stopped answering or a different boot answered; no
    /// stop was sent to any replacement server.
    OccupantChanged,
    /// The stop failed; the server may still be running.
    Failed(String),
}

/// Offers to restart the local server when it is a different build.
///
/// `probe` reads the running server's status (`None` when nothing answers),
/// `stop` stops the instance with the given boot identity and no other, and
/// `decide` is the operator's consent, asked only with a terminal. A stop that
/// finds a different boot has met a new occupant: the server is probed again,
/// and a still-different one is offered again, up to [`MAX_LOCAL_OFFERS`] times.
fn restart_local(
    can_prompt: bool,
    mut probe: impl FnMut() -> Option<RuntimeStatus>,
    mut stop: impl FnMut(&str) -> Result<(), ServerStopError>,
    mut decide: impl FnMut(&RuntimeStatus) -> bool,
) -> LocalRestart {
    let is_different = |status: &RuntimeStatus| !shepr_protocol::is_this_build(&status.build_id);
    let Some(mut observed) = probe().filter(is_different) else {
        return LocalRestart::NotNeeded;
    };
    if !can_prompt {
        return LocalRestart::NoTerminal;
    }
    for offer in 1..=MAX_LOCAL_OFFERS {
        if !decide(&observed) {
            return LocalRestart::Declined;
        }
        match stop(&observed.boot_id) {
            Ok(()) => return LocalRestart::Stopped,
            Err(ServerStopError::NotRunning { .. }) => return LocalRestart::OccupantChanged,
            Err(error) if error.is_boot_mismatch() => match probe().filter(is_different) {
                Some(next) if offer < MAX_LOCAL_OFFERS => observed = next,
                _ => return LocalRestart::OccupantChanged,
            },
            Err(error) => return LocalRestart::Failed(error.to_string()),
        }
    }
    LocalRestart::OccupantChanged
}

/// What the operator is told about the local server's restart. A server that
/// was kept running, or that no one could be asked about, is reported by the
/// launch that follows, with the stop command.
fn local_notice(local: &LocalRestart) -> Option<String> {
    match local {
        LocalRestart::NotNeeded | LocalRestart::NoTerminal | LocalRestart::Declined => None,
        LocalRestart::Stopped => Some(
            "shepr: stopped the local server of a different build; one of this build starts now."
                .to_owned(),
        ),
        LocalRestart::OccupantChanged => Some(
            "shepr: the local server changed while it was being stopped; no stop was sent to a new occupant."
                .to_owned(),
        ),
        LocalRestart::Failed(error) => {
            Some(format!("shepr: could not stop the local server: {error}"))
        }
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

/// The command an operator runs to stop a remote server themselves.
fn remote_stop_command(machine: &MachineConfig, server: &DifferentBuildServer) -> String {
    format!(
        "ssh {} {} server stop --expect-boot {}",
        shepr_remote::shell_quote(machine.ssh.as_str()),
        shepr_remote::shell_quote(server.executable.as_str()),
        shepr_remote::shell_quote(&server.boot_id)
    )
}

/// What is worth telling the operator after the preflight. Offline machines are
/// left to the client, which shows their state and retries; everything the
/// operator can act on is printed, including a machine that cannot be served
/// and a server of another build that was left running.
fn result_notices(
    machines: &[MachineConfig],
    outcomes: &[PreflightOutcome],
    can_prompt: bool,
) -> Vec<String> {
    let mut notices = Vec::new();
    for (machine, outcome) in machines.iter().zip(outcomes) {
        if let Some(Err(error)) = &outcome.authentication {
            notices.push(format!(
                "shepr: authentication for machine {} failed: {error}. The client keeps retrying it.",
                outcome.label
            ));
        }
        notices.extend(restart_notice(machine, outcome));
        match (&outcome.check, &outcome.authentication) {
            (MachineCheck::NeedsAuthentication(_), None) if !can_prompt => {
                notices.push(format!(
                    "shepr: machine {} needs authentication, but there is no terminal to prompt on; run shepr from an interactive terminal.",
                    outcome.label
                ));
            }
            (MachineCheck::NeedsAuthentication(diagnostic), Some(Ok(()))) => {
                notices.push(format!(
                    "shepr: machine {} still refuses the client's connection after ssh authenticated: {diagnostic}. The client keeps retrying it.",
                    outcome.label
                ));
            }
            (MachineCheck::HostKey(diagnostic), _) => {
                let mut notice = format!("shepr: machine {}: {diagnostic}", outcome.label);
                for hint in
                    shepr_remote::machine_ssh_error_hint(diagnostic, machine.ssh.as_str())
                {
                    notice.push('\n');
                    notice.push_str(&hint);
                }
                notices.push(notice);
            }
            (MachineCheck::Incompatible(diagnostic), _) => notices.push(format!(
                "shepr: machine {} cannot be used: {diagnostic}. The client shows it as unavailable and keeps retrying it.",
                outcome.label
            )),
            (MachineCheck::Failed(diagnostic), _) => {
                let client_action = if diagnostic.needs_attention() {
                    "The client shows it as unavailable and needs attention."
                } else {
                    "The client keeps retrying it."
                };
                let mut notice = format!(
                    "shepr: machine {} could not be checked: {diagnostic}. {client_action}",
                    outcome.label
                );
                for hint in shepr_remote::machine_ssh_error_hint(diagnostic, machine.ssh.as_str()) {
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

/// What came of the restart offer for one machine, with what to do when its
/// server of another build was left running.
fn restart_notice(machine: &MachineConfig, outcome: &PreflightOutcome) -> Option<String> {
    let restart = outcome.restart.as_ref()?;
    let label = &outcome.label;
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

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_remote::machine::{MachineLabel, RemoteExecutable};

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
            restart: None,
        }
    }

    fn diagnostic(message: &str) -> shepr_remote::SshFailureDiagnostic {
        shepr_remote::SshFailureDiagnostic::from_ssh_output(Some(255), message.into())
    }

    fn different_build_server() -> DifferentBuildServer {
        DifferentBuildServer {
            executable: RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition"),
            version: "0.0.0-old".into(),
            build_id: "ffffffffffffffff".into(),
            boot_id: "17-23".into(),
        }
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
            // The check that followed the prompt found the machine fine.
            outcome(&machines[2], MachineCheck::Ready, Some(Ok(()))),
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
        assert_eq!(notices.len(), 3, "{notices:?}");
        assert!(notices[0].contains("refused") && notices[0].contains("failed"));
        assert!(notices[1].contains("hostkey") && notices[1].contains("known_hosts"));
        assert!(
            notices[2].contains("incompatible") && notices[2].contains("another build"),
            "a machine of another build is no longer attached to in silence: {notices:?}"
        );
    }

    #[test]
    fn ssh_process_failures_are_reported_with_configuration_guidance() {
        let machines = [machine("config"), machine("closed")];
        let outcomes = [
            outcome(
                &machines[0],
                MachineCheck::Failed(diagnostic(
                    "/home/u/.ssh/config: line 12: Bad configuration option: hostkeyalgorithms",
                )),
                None,
            ),
            outcome(
                &machines[1],
                MachineCheck::Failed(diagnostic("Connection closed by host port 22")),
                None,
            ),
        ];

        let notices = result_notices(&machines, &outcomes, true);
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(
            notices[0].contains("Bad configuration option"),
            "{notices:?}"
        );
        assert!(
            notices[0].contains("local SSH configuration"),
            "{notices:?}"
        );
        assert!(notices[0].contains("needs attention"), "{notices:?}");
        assert!(
            notices[1].contains("Connection closed by host"),
            "{notices:?}"
        );
        assert!(notices[1].contains("needs attention"), "{notices:?}");
    }

    #[test]
    fn a_machine_still_refused_after_authenticating_is_named() {
        let machines = [machine("build")];
        let outcomes = [outcome(
            &machines[0],
            MachineCheck::NeedsAuthentication(diagnostic("Permission denied (publickey)")),
            Some(Ok(())),
        )];
        let notices = result_notices(&machines, &outcomes, true);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("still refuses"), "{notices:?}");
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

    fn restart_notices(restart: RestartResult, check: MachineCheck) -> Vec<String> {
        let machines = [machine("build")];
        let mut outcomes = [outcome(&machines[0], check, None)];
        outcomes[0].restart = Some(restart);
        result_notices(&machines, &outcomes, true)
    }

    #[test]
    fn a_server_left_running_names_the_stop_command_and_what_it_ends() {
        for (restart, extra) in [
            (RestartResult::Declined, ""),
            (RestartResult::NoTerminal, "interactive terminal"),
            (RestartResult::Failed("timed out".into()), "timed out"),
        ] {
            let notices = restart_notices(
                restart,
                MachineCheck::DifferentBuild(different_build_server()),
            );
            assert_eq!(notices.len(), 1, "{notices:?}");
            let notice = &notices[0];
            assert!(notice.contains("ffffffffffffffff"), "{notice}");
            assert!(
                notice
                    .contains("`ssh build.example /usr/bin/shepr server stop --expect-boot 17-23`"),
                "{notice}"
            );
            assert!(notice.contains("pane processes"), "{notice}");
            assert!(notice.contains("layout is restored"), "{notice}");
            assert!(notice.contains(extra), "{notice}");
            assert!(!notice.contains("--force"), "{notice}");
        }
    }

    #[test]
    fn a_stopped_server_and_a_changed_occupant_are_reported() {
        let stopped = restart_notices(RestartResult::Stopped, MachineCheck::Ready);
        assert_eq!(stopped.len(), 1, "{stopped:?}");
        assert!(stopped[0].contains("stopped"), "{stopped:?}");

        let changed = restart_notices(RestartResult::OccupantChanged, MachineCheck::Ready);
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert!(changed[0].contains("no stop was sent"), "{changed:?}");

        let replaced = restart_notices(
            RestartResult::OccupantChanged,
            MachineCheck::DifferentBuild(different_build_server()),
        );
        assert_eq!(replaced.len(), 1, "{replaced:?}");
        assert!(replaced[0].contains("Run shepr again"), "{replaced:?}");
    }

    #[test]
    fn the_offers_say_what_a_restart_ends_and_what_is_restored() {
        let remote = remote_offer(&machine("build"), &different_build_server());
        let local = local_offer(&status("ffffffffffffffff", "17-23"));
        for offer in [&remote, &local] {
            assert!(offer.contains("different build"), "{offer}");
            assert!(offer.contains("ends every pane process"), "{offer}");
            assert!(offer.contains("saved layout is restored"), "{offer}");
            assert!(offer.contains("agents are resumed"), "{offer}");
            assert!(offer.contains("[y/N]"), "the default is to keep: {offer}");
        }
        assert!(remote.contains("build (build.example)"), "{remote}");
    }

    #[test]
    fn consent_reads_only_through_the_answer_newline() {
        use std::io::Read as _;

        let mut input = io::Cursor::new(b"yes\nkeys typed ahead".as_slice());
        let answer = read_answer_line(|buffer| input.read(buffer))
            .expect("reading the answer succeeds")
            .expect("the answer is present");

        assert_eq!(answer, "yes");
        assert_eq!(input.position(), 4, "bytes after the answer stay unread");
    }

    #[test]
    fn only_an_explicit_yes_consents() {
        for yes in ["y", "Y", "yes", " YES \n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yep", "ok", "y n"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }

    fn status(build_id: &str, boot_id: &str) -> RuntimeStatus {
        RuntimeStatus {
            version: Some("0.0.0-test".into()),
            build_id: build_id.into(),
            boot_id: boot_id.into(),
            stopping: false,
            starting: false,
        }
    }

    fn other_build() -> &'static str {
        if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
            "0000000000000000"
        } else {
            "ffffffffffffffff"
        }
    }

    fn boot_mismatch() -> ServerStopError {
        ServerStopError::BootMismatch {
            label: "server".into(),
            expected_boot_id: "1-1".into(),
            detail: "it is boot 2-2".into(),
        }
    }

    /// Drives `restart_local` with a scripted sequence of probe answers and stop
    /// results, recording every question and stop.
    struct LocalScript {
        probes: Vec<Option<RuntimeStatus>>,
        stops: Vec<Result<(), ServerStopError>>,
        answers: Vec<bool>,
        asked: Vec<String>,
        stopped: Vec<String>,
    }

    impl LocalScript {
        fn new(
            probes: Vec<Option<RuntimeStatus>>,
            stops: Vec<Result<(), ServerStopError>>,
            answers: Vec<bool>,
        ) -> Self {
            Self {
                probes,
                stops,
                answers,
                asked: Vec::new(),
                stopped: Vec::new(),
            }
        }

        fn run(&mut self, can_prompt: bool) -> LocalRestart {
            let probes = std::cell::RefCell::new(std::mem::take(&mut self.probes));
            let stops = std::cell::RefCell::new(std::mem::take(&mut self.stops));
            let answers = std::cell::RefCell::new(std::mem::take(&mut self.answers));
            let asked = std::cell::RefCell::new(Vec::new());
            let stopped = std::cell::RefCell::new(Vec::new());
            let result = restart_local(
                can_prompt,
                || {
                    let mut probes = probes.borrow_mut();
                    if probes.is_empty() {
                        None
                    } else {
                        probes.remove(0)
                    }
                },
                |boot| {
                    stopped.borrow_mut().push(boot.to_owned());
                    stops.borrow_mut().remove(0)
                },
                |status| {
                    asked.borrow_mut().push(status.boot_id.clone());
                    answers.borrow_mut().remove(0)
                },
            );
            self.asked = asked.into_inner();
            self.stopped = stopped.into_inner();
            result
        }
    }

    #[test]
    fn a_local_server_of_this_build_or_none_needs_no_restart() {
        for probe in [None, Some(status(shepr_protocol::BUILD_ID, "1-1"))] {
            let mut script = LocalScript::new(vec![probe], vec![], vec![]);
            assert_eq!(script.run(true), LocalRestart::NotNeeded);
            assert!(script.asked.is_empty());
        }
    }

    #[test]
    fn a_local_different_build_is_restarted_with_consent() {
        let mut script = LocalScript::new(
            vec![Some(status(other_build(), "1-1"))],
            vec![Ok(())],
            vec![true],
        );
        assert_eq!(script.run(true), LocalRestart::Stopped);
        assert_eq!(script.asked, ["1-1"]);
        // The stop names the boot that was observed.
        assert_eq!(script.stopped, ["1-1"]);
    }

    #[test]
    fn a_refused_local_offer_stops_nothing() {
        let mut script = LocalScript::new(
            vec![Some(status(other_build(), "1-1"))],
            vec![],
            vec![false],
        );
        assert_eq!(script.run(true), LocalRestart::Declined);
        assert!(script.stopped.is_empty());
    }

    #[test]
    fn without_a_terminal_the_local_server_is_left_alone_unasked() {
        let mut script = LocalScript::new(vec![Some(status(other_build(), "1-1"))], vec![], vec![]);
        assert_eq!(script.run(false), LocalRestart::NoTerminal);
        assert!(script.asked.is_empty());
        assert!(script.stopped.is_empty());
    }

    #[test]
    fn a_replaced_local_occupant_is_rediscovered_and_offered_again() {
        let mut script = LocalScript::new(
            vec![
                Some(status(other_build(), "1-1")),
                Some(status(other_build(), "2-2")),
            ],
            vec![Err(boot_mismatch()), Ok(())],
            vec![true, true],
        );
        assert_eq!(script.run(true), LocalRestart::Stopped);
        assert_eq!(script.asked, ["1-1", "2-2"]);
        assert_eq!(script.stopped, ["1-1", "2-2"]);
    }

    #[test]
    fn a_local_occupant_that_no_longer_needs_a_restart_ends_the_offer() {
        for after in [None, Some(status(shepr_protocol::BUILD_ID, "2-2"))] {
            let mut script = LocalScript::new(
                vec![Some(status(other_build(), "1-1")), after],
                vec![Err(boot_mismatch())],
                vec![true],
            );
            assert_eq!(script.run(true), LocalRestart::OccupantChanged);
            assert_eq!(script.stopped, ["1-1"]);
        }
    }

    #[test]
    fn a_local_server_that_keeps_changing_is_offered_a_bounded_number_of_times() {
        let mut script = LocalScript::new(
            vec![
                Some(status(other_build(), "1-1")),
                Some(status(other_build(), "2-2")),
                Some(status(other_build(), "3-3")),
            ],
            vec![Err(boot_mismatch()), Err(boot_mismatch())],
            vec![true, true],
        );
        assert_eq!(script.run(true), LocalRestart::OccupantChanged);
        assert_eq!(script.asked.len(), MAX_LOCAL_OFFERS);
    }

    #[test]
    fn a_failed_local_stop_is_reported_with_its_reason() {
        let mut script = LocalScript::new(
            vec![Some(status(other_build(), "1-1"))],
            vec![Err(ServerStopError::Protocol("refused".into()))],
            vec![true],
        );
        assert_eq!(script.run(true), LocalRestart::Failed("refused".into()));
        let notice = local_notice(&LocalRestart::Failed("refused".into())).expect("a notice");
        assert!(notice.contains("refused"), "{notice}");
    }

    #[test]
    fn the_local_notices_leave_kept_servers_to_the_launch() {
        for kept in [
            LocalRestart::NotNeeded,
            LocalRestart::NoTerminal,
            LocalRestart::Declined,
        ] {
            assert_eq!(local_notice(&kept), None);
        }
        assert!(local_notice(&LocalRestart::Stopped).is_some());
        assert!(local_notice(&LocalRestart::OccupantChanged).is_some());
    }
}
