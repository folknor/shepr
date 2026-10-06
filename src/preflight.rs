//! The startup step before the TUI takes the terminal. The TUI connects with
//! `BatchMode=yes`, so two things that need the operator happen here first:
//!
//! - machines that need an SSH prompt get it, one at a time, and are checked
//!   again afterwards;
//! - a running local server of another build is offered a restart, after the
//!   last authentication prompt, so prompts and questions never interleave.
//!
//! A configured machine's server of another build gets no question here: the
//! client never starts or restarts a server on another host by itself, and
//! shows such a machine with a Restart entry the operator can choose.
//!
//! The machine mechanism lives in `shepr_remote::preflight` and the restart
//! offer in `shepr_launch::restart`; this module supplies the config, the
//! terminal and the local server, and `words` the operator text, the hints for
//! a failed machine check included.

use std::io::{self, IsTerminal, Write as _};
use std::os::fd::AsRawFd as _;

use shepr_config::MachineConfig;
use shepr_launch::restart::{RestartDecision, RestartResult, StopOutcome};
use shepr_launch::status::RuntimeStatus;
use shepr_launch::stop::ServerStopError;

mod words;
use words::{local_notice, local_offer, prompt_notice, result_notices};

/// Authenticates the machines that need it, then offers to restart a running
/// local server of another build. Every question needs a terminal; without one
/// nothing is asked and nothing is stopped.
///
/// Unreachable machines are not an error here: the client shows them offline
/// and keeps retrying. Host keys are never accepted; a machine whose key is
/// unknown or changed is named so the operator can fix it. A local server left
/// running is reported by the launch that follows.
pub(crate) fn run(
    config: &shepr_config::ValidatedClientConfig,
    paths: &shepr_paths::AppPaths,
) -> Vec<shepr_remote::MachineSshConnector> {
    // Questions go to stderr and answers come from stdin, and ssh prompts use
    // the terminal too, so asking needs both to be a terminal.
    let can_prompt = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let machines = config.machines();
    let has_remote_machines = !machines.is_empty();
    let ssh = shepr_remote::MachineSshPreflight::new(paths);
    let mut before_authentication = |machine: &MachineConfig| {
        crate::cli::print_notice(&prompt_notice(machine));
    };
    let authentication_prompt: Option<&mut dyn FnMut(&MachineConfig)> = if can_prompt {
        Some(&mut before_authentication)
    } else {
        None
    };
    let (outcomes, connectors) = ssh.run(machines, authentication_prompt);

    let mut decide_local = |status: &RuntimeStatus| {
        if confirm(&local_offer(status)) {
            crate::cli::print_notice(&"shepr: stopping the local server.");
            RestartDecision::Restart
        } else {
            RestartDecision::Keep
        }
    };
    let local_decision: Option<&mut dyn FnMut(&RuntimeStatus) -> RestartDecision> = if can_prompt {
        Some(&mut decide_local)
    } else {
        None
    };
    let local = restart_local(
        || local_server_status(paths, has_remote_machines),
        |boot_id| shepr_launch::stop::stop_for_startup_restart(paths, boot_id),
        local_decision,
    );
    for notice in local_notice(&local)
        .into_iter()
        .chain(result_notices(&outcomes, can_prompt))
    {
        crate::cli::print_notice(&notice);
    }
    connectors
}

/// The status of a running local server of any build, when this client can
/// start its replacement. A socket override names an existing server but is
/// not an address this client can launch for, so it gets no restart offer. A
/// server that cannot be read is reported here when remote machines let the
/// TUI continue; otherwise the later local launch prints its refusal.
fn local_server_status(
    paths: &shepr_paths::AppPaths,
    report_unavailable: bool,
) -> Option<RuntimeStatus> {
    if !paths.server_address().is_runtime_address() {
        return None;
    }
    let announce_wait = || {
        crate::cli::print_notice(
            &"shepr: the local server is starting; waiting for it to be ready.",
        );
    };
    match shepr_launch::local_server::running_server_status(paths, announce_wait) {
        Ok(status) => status,
        Err(error) => {
            shepr_platform::structured_log!(WARN, event = launch.restart_offer, outcome = "unavailable", socket = %paths.server_address().socket().display(), error_kind = ?error.kind(), %error, "no restart offer: cannot read the local server");
            if report_unavailable {
                crate::cli::print_notice(&format!(
                    "shepr: cannot check the local server for a restart: {error}"
                ));
            }
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

/// Offers to restart the local server when it is a different build.
///
/// `probe` reads the running server's status (`None` when nothing answers),
/// `stop` stops the instance with the given boot identity and no other, and
/// `decide` is the operator's consent when present. A stop that
/// finds a different boot has met a new occupant: the server is probed again,
/// and a still-different one is offered again, up to the local offer limit.
fn restart_local(
    mut probe: impl FnMut() -> Option<RuntimeStatus>,
    mut stop: impl FnMut(&shepr_protocol::BootId) -> Result<(), ServerStopError>,
    decide: Option<&mut dyn FnMut(&RuntimeStatus) -> RestartDecision>,
) -> RestartResult {
    RestartResult::offer(
        decide,
        &mut probe,
        |status| !status.build_id.is_this_build(),
        |status| match stop(&status.boot_id) {
            Ok(()) => Ok(StopOutcome::Stopped),
            Err(ServerStopError::NotRunning { .. }) => Ok(StopOutcome::NoServer),
            Err(error) if error.is_boot_mismatch() => Ok(StopOutcome::BootChanged),
            Err(error) => Err(error),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::words::*;
    use super::*;
    use shepr_config::SshTarget;
    use shepr_launch::{EndpointFailure, SshFailureClass};
    use shepr_remote::machine::MachineLabel;
    use shepr_remote::{MachineCheck, PreflightOutcome};

    fn machine(label: &str) -> MachineConfig {
        MachineConfig {
            label: MachineLabel::parse(label).expect("test precondition"),
            ssh: shepr_config::SshTarget::parse(format!("{label}.example"))
                .expect("test precondition"),
            palette: shepr_config::DEFAULT_LOCAL_HUE,
        }
    }

    fn outcome(
        machine: &MachineConfig,
        check: MachineCheck,
        authentication: Option<Result<(), shepr_remote::AuthenticationError>>,
    ) -> PreflightOutcome {
        PreflightOutcome {
            machine: machine.clone(),
            check,
            authentication,
        }
    }

    fn ssh_failure(class: SshFailureClass, message: &str) -> EndpointFailure {
        EndpointFailure::ssh(class, message)
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
                MachineCheck::Offline(ssh_failure(SshFailureClass::Link, "Connection refused")),
                None,
            ),
            // The check that followed the prompt found the machine fine.
            outcome(&machines[2], MachineCheck::Ready, Some(Ok(()))),
            outcome(
                &machines[3],
                MachineCheck::NeedsAuthentication(ssh_failure(
                    SshFailureClass::Authentication,
                    "Permission denied (publickey)",
                )),
                Some(Err(shepr_remote::AuthenticationError::Exited(
                    std::os::unix::process::ExitStatusExt::from_raw(255 << 8),
                ))),
            ),
            outcome(
                &machines[4],
                MachineCheck::HostKey(ssh_failure(
                    SshFailureClass::HostKey,
                    "Host key verification failed.",
                )),
                None,
            ),
            outcome(
                &machines[5],
                MachineCheck::Incompatible(EndpointFailure::incompatible("another build")),
                None,
            ),
        ];
        let notices = result_notices(&outcomes, true);
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
                MachineCheck::Failed(ssh_failure(
                    SshFailureClass::Configuration,
                    "/home/u/.ssh/config: line 12: Bad configuration option: hostkeyalgorithms",
                )),
                None,
            ),
            outcome(
                &machines[1],
                MachineCheck::Failed(ssh_failure(
                    SshFailureClass::RemoteRejected,
                    "Connection closed by host port 22",
                )),
                None,
            ),
        ];

        let notices = result_notices(&outcomes, true);
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
    fn hints_follow_the_typed_cause_and_name_the_configured_target() {
        let target = SshTarget::parse("host name").expect("test precondition");
        for message in [
            "Host key verification failed.",
            "REMOTE HOST IDENTIFICATION HAS CHANGED!",
        ] {
            let hints =
                machine_failure_hints(&ssh_failure(SshFailureClass::HostKey, message), &target);
            assert_eq!(hints.len(), 1, "{message}");
            assert!(hints[0].contains("known_hosts"), "{hints:?}");
        }

        for message in [
            "remote platform detection failed: user@host: Permission denied (publickey).",
            "remote server status failed: user@host: Permission denied (keyboard-interactive).",
            "SIGN_AND_SEND_PUBKEY: SIGNING FAILED for ED25519 from agent: agent refused operation",
        ] {
            let hints = machine_failure_hints(
                &ssh_failure(SshFailureClass::Authentication, message),
                &target,
            );
            assert_eq!(hints.len(), 2, "{message}");
            assert!(
                hints[0].contains("`ssh 'host name'`"),
                "the target is quoted: {hints:?}"
            );
            assert!(hints[1].contains("ssh-add"), "{hints:?}");
        }

        let configuration = machine_failure_hints(
            &ssh_failure(
                SshFailureClass::Configuration,
                "Bad owner or permissions on /home/u/.ssh/config",
            ),
            &target,
        );
        assert!(
            configuration[0].contains("local SSH configuration"),
            "{configuration:?}"
        );

        // A host-key refusal named alongside a denied key is a host-key failure.
        let both = machine_failure_hints(
            &ssh_failure(
                SshFailureClass::HostKey,
                "Permission denied (publickey). Host key verification failed.",
            ),
            &target,
        );
        assert!(
            both.iter().all(|hint| !hint.contains("ssh-add")),
            "{both:?}"
        );

        for failure in [
            EndpointFailure::unclassified("server closed connection"),
            EndpointFailure::incompatible("remote platform detection failed: unsupported platform"),
            EndpointFailure::from_error(&io::Error::from(io::ErrorKind::TimedOut)),
        ] {
            assert!(
                machine_failure_hints(&failure, &target).is_empty(),
                "{failure}"
            );
        }
    }

    #[test]
    fn a_machine_still_refused_after_authenticating_is_named() {
        let machines = [machine("build")];
        let outcomes = [outcome(
            &machines[0],
            MachineCheck::NeedsAuthentication(ssh_failure(
                SshFailureClass::Authentication,
                "Permission denied (publickey)",
            )),
            Some(Ok(())),
        )];
        let notices = result_notices(&outcomes, true);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("still refuses"), "{notices:?}");
    }

    #[test]
    fn a_machine_needing_authentication_without_a_terminal_is_named() {
        let machines = [machine("build")];
        let outcomes = [outcome(
            &machines[0],
            MachineCheck::NeedsAuthentication(ssh_failure(
                SshFailureClass::Authentication,
                "Permission denied (publickey)",
            )),
            None,
        )];
        assert!(result_notices(&outcomes, true).is_empty());
        let notices = result_notices(&outcomes, false);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("no terminal"), "{notices:?}");
    }

    #[test]
    fn the_local_offer_says_what_a_restart_ends_and_what_is_restored() {
        let offer = local_offer(&status("ffffffffffffffff", "17-23"));
        assert!(offer.contains("different build"), "{offer}");
        assert!(offer.contains("ends every pane process"), "{offer}");
        assert!(offer.contains("saved layout is restored"), "{offer}");
        assert!(offer.contains("agents are resumed"), "{offer}");
        assert!(offer.contains("[y/N]"), "the default is to keep: {offer}");
    }

    #[test]
    fn local_restart_identity_controls_are_rejected_before_the_offer() {
        for (build_id, boot_id) in [
            ("build\u{1b}[2J", "17-23"),
            ("0123456789abcdef", "boot\nforged"),
        ] {
            let value = serde_json::json!({
                "id": "status",
                "result": {
                    "type": "pong", "version": "1.0", "build_id": build_id,
                    "boot_id": boot_id, "starting": false, "stopping": false,
                },
            });
            assert!(serde_json::from_value::<shepr_api::schema::SuccessResponse>(value).is_err());
        }
    }

    #[test]
    fn local_restart_probe_waits_for_a_starting_server_to_become_running() {
        use crate::test_support::IsolatedEnv;
        use std::io::{BufRead as _, Write as _};
        use std::os::unix::net::UnixListener;

        let _env = IsolatedEnv::new();
        let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
        shepr_platform::create_private_runtime_directory(paths.runtime_dir())
            .expect("runtime directory");
        let socket = paths.server_address().socket().to_path_buf();
        let listener = UnixListener::bind(&socket).expect("bind server socket");
        let expected_build = other_build().to_owned();
        let server = std::thread::spawn(move || {
            for starting in [true, false] {
                loop {
                    let (mut stream, _) = listener.accept().expect("accept probe");
                    let mut line = String::new();
                    let mut reader =
                        std::io::BufReader::new(stream.try_clone().expect("clone status stream"));
                    reader.read_line(&mut line).expect("read status request");
                    // The liveness check connects and closes before the ping.
                    if line.is_empty() {
                        continue;
                    }
                    let request: serde_json::Value =
                        serde_json::from_str(&line).expect("status request JSON");
                    let response = serde_json::json!({
                        "id": request["id"],
                        "result": {
                            "type": "pong",
                            "version": "0.1.0-test",
                            "build_id": expected_build.as_str(),
                            "boot_id": "17-23",
                            "stopping": false,
                            "starting": starting,
                        },
                    });
                    writeln!(stream, "{response}").expect("write status response");
                    break;
                }
            }
        });

        let status =
            local_server_status(&paths, true).expect("starting server settles to a running status");
        server.join().expect("fake server thread");
        assert_eq!(status.build_id.to_string(), other_build());
        assert_eq!(
            status.lifecycle,
            shepr_launch::status::RuntimeLifecycle::Running
        );
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
            version: "0.0.0-test".into(),
            build_id: build_id.parse().expect("build identity"),
            boot_id: boot_id.parse().expect("boot identity"),
            lifecycle: shepr_launch::status::RuntimeLifecycle::Running,
        }
    }

    use shepr_test_fixtures::other_build_id as other_build;

    fn boot_mismatch() -> ServerStopError {
        ServerStopError::BootMismatch {
            expected_boot_id: "1-1".parse().expect("boot identity"),
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

        fn run(&mut self, can_prompt: bool) -> RestartResult {
            let probes = std::cell::RefCell::new(std::mem::take(&mut self.probes));
            let stops = std::cell::RefCell::new(std::mem::take(&mut self.stops));
            let answers = std::cell::RefCell::new(std::mem::take(&mut self.answers));
            let asked = std::cell::RefCell::new(Vec::new());
            let stopped = std::cell::RefCell::new(Vec::new());
            let result = {
                let mut decide = |status: &RuntimeStatus| {
                    asked.borrow_mut().push(status.boot_id.to_string());
                    if answers.borrow_mut().remove(0) {
                        RestartDecision::Restart
                    } else {
                        RestartDecision::Keep
                    }
                };
                let prompt: Option<&mut dyn FnMut(&RuntimeStatus) -> RestartDecision> =
                    if can_prompt { Some(&mut decide) } else { None };
                restart_local(
                    || {
                        let mut probes = probes.borrow_mut();
                        if probes.is_empty() {
                            None
                        } else {
                            probes.remove(0)
                        }
                    },
                    |boot| {
                        stopped.borrow_mut().push(boot.to_string());
                        stops.borrow_mut().remove(0)
                    },
                    prompt,
                )
            };
            self.asked = asked.into_inner();
            self.stopped = stopped.into_inner();
            result
        }
    }

    #[test]
    fn a_local_server_of_this_build_or_none_needs_no_restart() {
        for probe in [None, Some(status(shepr_protocol::BUILD_ID, "1-1"))] {
            let mut script = LocalScript::new(vec![probe], vec![], vec![]);
            assert!(matches!(script.run(true), RestartResult::NotNeeded));
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
        assert!(matches!(script.run(true), RestartResult::Stopped));
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
        assert!(matches!(script.run(true), RestartResult::Declined));
        assert!(script.stopped.is_empty());
    }

    #[test]
    fn without_a_terminal_the_local_server_is_left_alone_unasked() {
        let mut script = LocalScript::new(vec![Some(status(other_build(), "1-1"))], vec![], vec![]);
        assert!(matches!(script.run(false), RestartResult::NoTerminal));
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
        assert!(matches!(script.run(true), RestartResult::Stopped));
        assert_eq!(script.asked, ["1-1", "2-2"]);
        assert_eq!(script.stopped, ["1-1", "2-2"]);
    }

    #[test]
    fn a_local_occupant_that_no_longer_needs_a_restart_ends_the_offer() {
        for after in [None, Some(status(shepr_protocol::BUILD_ID, "2-2"))] {
            let expected = if after.is_none() {
                RestartResult::NoServer
            } else {
                RestartResult::OccupantChanged
            };
            let mut script = LocalScript::new(
                vec![Some(status(other_build(), "1-1")), after],
                vec![Err(boot_mismatch())],
                vec![true],
            );
            assert_eq!(
                std::mem::discriminant(&script.run(true)),
                std::mem::discriminant(&expected)
            );
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
        assert!(matches!(script.run(true), RestartResult::OccupantChanged));
        assert_eq!(script.asked.len(), shepr_launch::limits::MAX_RESTART_OFFERS);
    }

    #[test]
    fn a_local_server_gone_before_its_stop_is_no_server_not_a_new_occupant() {
        let mut script = LocalScript::new(
            vec![Some(status(other_build(), "1-1"))],
            vec![Err(ServerStopError::NotRunning {
                path: "/run/shepr/server.sock".into(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            })],
            vec![true],
        );
        assert!(matches!(script.run(true), RestartResult::NoServer));
        assert_eq!(script.stopped, ["1-1"]);
        let notice = local_notice(&RestartResult::NoServer).expect("a notice");
        assert!(notice.contains("already stopped"), "{notice}");
    }

    #[test]
    fn a_failed_local_stop_is_reported_with_its_reason() {
        let mut script = LocalScript::new(
            vec![Some(status(other_build(), "1-1"))],
            vec![Err(ServerStopError::Protocol("refused".into()))],
            vec![true],
        );
        assert!(
            matches!(script.run(true), RestartResult::Failed(ServerStopError::Protocol(detail)) if detail == "refused")
        );
        let notice = local_notice(&RestartResult::Failed(ServerStopError::Protocol(
            "refused".into(),
        )))
        .expect("a notice");
        assert!(notice.contains("refused"), "{notice}");
    }

    #[test]
    fn the_local_notices_leave_kept_servers_to_the_launch() {
        for kept in [
            RestartResult::NotNeeded,
            RestartResult::NoTerminal,
            RestartResult::Declined,
        ] {
            assert_eq!(local_notice(&kept), None);
        }
        assert!(local_notice(&RestartResult::Stopped).is_some());
        assert!(local_notice(&RestartResult::OccupantChanged).is_some());
    }
}
