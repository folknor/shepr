//! Startup authentication and restart offers for configured machines.
//!
//! The TUI reaches machines with `BatchMode=yes`, so any prompt (password, key
//! passphrase, keyboard-interactive, FIDO touch) fails its connection. This
//! step runs once before the client takes over the terminal:
//!
//! 1. [`preflight`] checks every machine concurrently and without prompting,
//!    walks the ones that need authentication one at a time and runs interactive
//!    ssh on shepr's own control socket for each, then checks those machines
//!    again, since the first check could not see past the prompt.
//!    `ControlPersist` keeps the authenticated master alive after that ssh
//!    exits, so the client's connectors reuse it.
//! 2. [`restart_different_builds`] offers, one machine at a time, to restart a
//!    running server of another build on a machine that passed. It runs after
//!    every authentication prompt so prompts never interleave with offers.
//!
//! The orchestration talks to ssh only through [`PreflightSsh`], so its
//! parallelism, prompt serialization, classification and restart decisions are
//! tested without a host. This crate does not print or read the terminal: the
//! caller announces each prompt through the `before_authentication` callback,
//! asks each restart question through the `decide` callback, and reports the
//! returned outcomes.

use std::io;
use std::sync::Mutex;
use std::time::Instant;

use crate::SshFailureDiagnostic;
use crate::machine::{MachineConfig, MachineLabel};
use crate::{DifferentBuildServer, MachineSshCheck, RemoteStop};

use crate::limits::MAX_RESTART_OFFERS;

/// The SSH operations the preflight needs.
pub trait PreflightSsh: Sync {
    /// Called before each round of concurrent checks, so an implementation can
    /// give the round its own time budget: the re-check after a prompt starts
    /// long after the first check did.
    fn start_round(&self) {}

    /// Checks one machine without prompting. Called for all machines of a round
    /// at once, from separate threads.
    fn check(&self, machine: &MachineConfig) -> io::Result<MachineSshCheck>;

    /// Runs interactive authentication for one machine in this terminal. Called
    /// for one machine at a time, from the calling thread.
    fn authenticate(&self, machine: &MachineConfig) -> io::Result<()>;

    /// Stops the server instance `server` describes, if it is still the one
    /// running on the machine. Called for one machine at a time, from the
    /// calling thread, only after the caller consented.
    fn stop_server(
        &self,
        machine: &MachineConfig,
        server: &DifferentBuildServer,
    ) -> io::Result<RemoteStop>;
}

/// What the non-interactive check found out about one machine.
#[derive(Clone, Debug)]
pub enum MachineCheck {
    /// SSH works and a compatible shepr can be served from the machine.
    Ready,
    /// SSH refused the credentials the TUI can use; a prompt may fix it.
    NeedsAuthentication(SshFailureDiagnostic),
    /// The machine did not answer: timeout, refusal, no route.
    Offline(SshFailureDiagnostic),
    /// The host key is unknown or changed. Never accepted automatically.
    HostKey(SshFailureDiagnostic),
    /// The installed pair is this build and a server of another build is
    /// running: a restart would fix it, with the operator's consent.
    DifferentBuild(DifferentBuildServer),
    /// The machine answered but cannot be served: no shepr, another build, a
    /// shepr-server beside it that is missing or another build, or a running
    /// server whose build or boot identity is unknown.
    Incompatible(SshFailureDiagnostic),
    /// Any other failure.
    Failed(SshFailureDiagnostic),
}

impl MachineCheck {
    pub fn needs_authentication(&self) -> bool {
        matches!(self, Self::NeedsAuthentication(_))
    }
}

/// Sorts a check result into the classes the preflight acts on.
pub fn classify_check(result: io::Result<MachineSshCheck>) -> MachineCheck {
    let error = match result {
        Ok(MachineSshCheck::Ready) => return MachineCheck::Ready,
        Ok(MachineSshCheck::DifferentBuild(server)) => return MachineCheck::DifferentBuild(server),
        Err(error) => error,
    };
    let diagnostic = SshFailureDiagnostic::from_error(&error);
    if diagnostic.requires_authentication() {
        MachineCheck::NeedsAuthentication(diagnostic)
    } else if diagnostic.is_host_key() {
        MachineCheck::HostKey(diagnostic)
    } else if diagnostic.is_transient_network_failure() {
        MachineCheck::Offline(diagnostic)
    } else if diagnostic.is_ssh_process_failure() {
        // Exit 255 alone cannot establish a transient link failure. Keep the
        // diagnostic visible when OpenSSH reports an unknown or actionable cause.
        MachineCheck::Failed(diagnostic)
    } else if diagnostic.needs_attention() {
        MachineCheck::Incompatible(diagnostic)
    } else {
        MachineCheck::Failed(diagnostic)
    }
}

/// How the offer to restart a machine's server of another build ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartResult {
    /// There was no terminal to ask on, so the server was left running.
    NoTerminal,
    /// The operator kept the server running.
    Declined,
    /// The server was stopped; the bridge starts one of this build on attach.
    Stopped,
    /// The server that answered the stop was not the one observed (it was
    /// restarted in between), so nothing was stopped. The outcome's check is
    /// the fresh one.
    OccupantChanged,
    /// The stop failed; the server may still be running.
    Failed(String),
}

/// The answer to one restart offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartDecision {
    Restart,
    Keep,
}

/// How one machine came out of the preflight.
#[derive(Debug)]
pub struct PreflightOutcome {
    pub label: MachineLabel,
    /// The latest non-interactive check: the one before any prompt, or the
    /// check that followed a successful prompt, or the one that followed a
    /// restart.
    pub check: MachineCheck,
    /// `None` when no prompt was run for this machine; otherwise whether the
    /// interactive ssh succeeded, with its failure text.
    pub authentication: Option<Result<(), String>>,
    /// `None` until a restart of this machine's server was considered.
    pub restart: Option<RestartResult>,
}

/// Checks every machine concurrently, then authenticates the ones that need it
/// one after another, in configuration order, then checks those again.
/// `before_authentication` runs just before each prompt so the caller can say
/// which machine it is for. With `can_prompt` false no prompt runs at all, and
/// machines needing authentication are reported as such.
///
/// Outcomes come back in the order of `machines`. A machine whose prompt
/// succeeded carries the check that followed it; one whose prompt failed keeps
/// the check that asked for it. Restarts are a separate step
/// ([`restart_different_builds`]).
pub fn preflight(
    machines: &[MachineConfig],
    ssh: &dyn PreflightSsh,
    can_prompt: bool,
    mut before_authentication: impl FnMut(&MachineConfig),
) -> Vec<PreflightOutcome> {
    let all: Vec<&MachineConfig> = machines.iter().collect();
    let checks = check_concurrently(ssh, &all);

    let mut outcomes: Vec<PreflightOutcome> = machines
        .iter()
        .zip(checks)
        .map(|(machine, check)| {
            let authentication = if can_prompt && check.needs_authentication() {
                before_authentication(machine);
                Some(ssh.authenticate(machine).map_err(|error| error.to_string()))
            } else {
                None
            };
            PreflightOutcome {
                label: machine.label.clone(),
                check,
                authentication,
                restart: None,
            }
        })
        .collect();

    // The first check stopped at the authentication failure, so it never saw the
    // machine's shepr or server. Look again, now that the master is open.
    let authenticated: Vec<usize> = outcomes
        .iter()
        .enumerate()
        .filter(|(_, outcome)| matches!(outcome.authentication, Some(Ok(()))))
        .map(|(index, _)| index)
        .collect();
    if !authenticated.is_empty() {
        let rechecked: Vec<&MachineConfig> = authenticated
            .iter()
            .map(|&index| &machines[index])
            .collect();
        let checks = check_concurrently(ssh, &rechecked);
        for (index, check) in authenticated.into_iter().zip(checks) {
            outcomes[index].check = check;
        }
    }
    outcomes
}

/// One round of checks, all machines at once.
fn check_concurrently(ssh: &dyn PreflightSsh, machines: &[&MachineConfig]) -> Vec<MachineCheck> {
    ssh.start_round();
    std::thread::scope(|scope| {
        let handles: Vec<_> = machines
            .iter()
            .map(|machine| scope.spawn(move || classify_check(ssh.check(machine))))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| {
                    MachineCheck::Failed(SshFailureDiagnostic::from_message(
                        "the machine check panicked",
                    ))
                })
            })
            .collect()
    })
}

/// Offers, one machine at a time in configuration order, to restart the running
/// server of another build on each machine whose check says so, and records what
/// came of it in the outcome's `restart` (and `check`, when the machine changed).
/// `outcomes` pair with `machines` by position, as [`preflight`] returns them.
///
/// `decide` is the operator's consent and is asked only with a terminal
/// (`can_prompt`); without one the server is left alone, whatever the default
/// answer would be. Nothing is stopped without a [`RestartDecision::Restart`].
///
/// The stop names the boot identity that was observed, and the remote refuses it
/// for any other instance. When it does, the occupant changed: the machine is
/// checked again (back to discovery) and a still-different server is offered
/// again, up to [`MAX_RESTART_OFFERS`] times. A stopped machine's check becomes
/// [`MachineCheck::Ready`], since the bridge starts a server of this build on
/// attach.
pub fn restart_different_builds(
    machines: &[MachineConfig],
    outcomes: &mut [PreflightOutcome],
    ssh: &dyn PreflightSsh,
    can_prompt: bool,
    mut decide: impl FnMut(&MachineConfig, &DifferentBuildServer) -> RestartDecision,
) {
    for (machine, outcome) in machines.iter().zip(outcomes.iter_mut()) {
        let MachineCheck::DifferentBuild(observed) = &outcome.check else {
            continue;
        };
        if !can_prompt {
            outcome.restart = Some(RestartResult::NoTerminal);
            continue;
        }
        let mut server = observed.clone();
        for offer in 1..=MAX_RESTART_OFFERS {
            if decide(machine, &server) == RestartDecision::Keep {
                outcome.restart = Some(RestartResult::Declined);
                break;
            }
            match ssh.stop_server(machine, &server) {
                Ok(RemoteStop::Stopped) => {
                    outcome.check = MachineCheck::Ready;
                    outcome.restart = Some(RestartResult::Stopped);
                    break;
                }
                Err(error) => {
                    outcome.restart = Some(RestartResult::Failed(error.to_string()));
                    break;
                }
                Ok(RemoteStop::BootChanged) => {
                    outcome.restart = Some(RestartResult::OccupantChanged);
                    outcome.check = check_concurrently(ssh, &[machine])
                        .pop()
                        .unwrap_or(MachineCheck::Ready);
                    match &outcome.check {
                        MachineCheck::DifferentBuild(next) if offer < MAX_RESTART_OFFERS => {
                            server = next.clone();
                        }
                        _ => break,
                    }
                }
            }
        }
    }
}

/// The real ssh behind [`PreflightSsh`]: [`check_machine_ssh`](crate::check_machine_ssh)
/// under one shared deadline per round, `ssh_authentication_command` on shepr's
/// control socket, and [`stop_remote_server`](crate::stop_remote_server).
pub struct MachineSshPreflight<'a> {
    paths: &'a shepr_config::AppPaths,
    deadline: Mutex<Instant>,
}

impl<'a> MachineSshPreflight<'a> {
    /// The deadline for the first round of checks starts now.
    pub fn new(paths: &'a shepr_config::AppPaths) -> Self {
        Self {
            paths,
            deadline: Mutex::new(round_deadline()),
        }
    }
}

fn round_deadline() -> Instant {
    // clock-io-ok: the deadline bounds real ssh IO for one round of checks.
    Instant::now() + crate::limits::PREFLIGHT_CHECK_BUDGET
}

impl PreflightSsh for MachineSshPreflight<'_> {
    fn start_round(&self) {
        let mut deadline = self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *deadline = round_deadline();
    }

    fn check(&self, machine: &MachineConfig) -> io::Result<MachineSshCheck> {
        let deadline = *self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::check_machine_ssh(self.paths, &machine.ssh, deadline)
    }

    fn authenticate(&self, machine: &MachineConfig) -> io::Result<()> {
        // The command's owner stays alive until the child has exited: OpenSSH
        // reads its temporary config after spawn.
        let mut authentication = crate::ssh_authentication_command(self.paths, &machine.ssh)?;
        let status = authentication.command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("ssh exited with {status}")))
        }
    }

    fn stop_server(
        &self,
        machine: &MachineConfig,
        server: &DifferentBuildServer,
    ) -> io::Result<RemoteStop> {
        crate::stop_remote_server(self.paths, &machine.ssh, server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::RemoteExecutable;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn machine(label: &str) -> MachineConfig {
        MachineConfig {
            label: MachineLabel::parse(label).expect("test precondition"),
            ssh: crate::SshTarget::parse(format!("{label}.example")).expect("test precondition"),
        }
    }

    fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().expect("test precondition")
    }

    fn ssh_failure(message: &str) -> io::Error {
        io::Error::other(SshFailureDiagnostic::from_ssh_output(
            Some(crate::SSH_OWN_FAILURE_EXIT_CODE),
            message.into(),
        ))
    }

    fn server(boot_id: &str) -> DifferentBuildServer {
        DifferentBuildServer {
            executable: RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition"),
            version: "0.0.0-old".into(),
            build_id: "ffffffffffffffff".into(),
            boot_id: boot_id.into(),
        }
    }

    /// What a scripted stop does.
    enum StopScript {
        Stopped,
        /// Another instance answered: it has this boot identity from now on.
        ChangedTo(&'static str),
        /// Another instance answered, and by the next check nothing needs a
        /// restart.
        ChangedToReady,
        Fails,
    }

    /// A scripted ssh: each machine's check result is looked up by label, and
    /// every call is logged with how many like calls were in flight.
    ///
    /// The label `stale` is a machine running a server of another build with the
    /// boot identity in `current_boot`; `authstale` is one that needs a prompt
    /// and shows that server only once authenticated.
    struct FakeSsh {
        needs_authentication: Vec<&'static str>,
        failing_authentication: Vec<&'static str>,
        authenticated: Mutex<Vec<String>>,
        current_boot: Mutex<String>,
        ready_now: Mutex<bool>,
        stops: Mutex<VecDeque<StopScript>>,
        check_counts: Mutex<HashMap<String, usize>>,
        rounds: AtomicUsize,
        checks_active: AtomicUsize,
        max_checks_active: AtomicUsize,
        prompts_active: AtomicUsize,
        max_prompts_active: AtomicUsize,
        log: Mutex<Vec<String>>,
    }

    impl FakeSsh {
        fn new(needs_authentication: &[&'static str]) -> Self {
            Self {
                needs_authentication: needs_authentication.to_vec(),
                failing_authentication: Vec::new(),
                authenticated: Mutex::new(Vec::new()),
                current_boot: Mutex::new("boot-1".into()),
                ready_now: Mutex::new(false),
                stops: Mutex::new(VecDeque::new()),
                check_counts: Mutex::new(HashMap::new()),
                rounds: AtomicUsize::new(0),
                checks_active: AtomicUsize::new(0),
                max_checks_active: AtomicUsize::new(0),
                prompts_active: AtomicUsize::new(0),
                max_prompts_active: AtomicUsize::new(0),
                log: Mutex::new(Vec::new()),
            }
        }

        fn with_stops(self, stops: impl IntoIterator<Item = StopScript>) -> Self {
            *self.stops.lock().expect("test precondition") = stops.into_iter().collect();
            self
        }

        fn log(&self, entry: String) {
            self.log.lock().expect("test precondition").push(entry);
        }

        fn entries(&self) -> Vec<String> {
            self.log.lock().expect("test precondition").clone()
        }

        fn checks_of(&self, label: &str) -> usize {
            self.check_counts
                .lock()
                .expect("test precondition")
                .get(label)
                .copied()
                .unwrap_or(0)
        }
    }

    fn enter(active: &AtomicUsize, max: &AtomicUsize) {
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(now, Ordering::SeqCst);
    }

    impl PreflightSsh for FakeSsh {
        fn start_round(&self) {
            self.rounds.fetch_add(1, Ordering::SeqCst);
        }

        fn check(&self, machine: &MachineConfig) -> io::Result<MachineSshCheck> {
            enter(&self.checks_active, &self.max_checks_active);
            // Long enough for every concurrent check to be in flight together.
            std::thread::sleep(Duration::from_millis(100));
            self.checks_active.fetch_sub(1, Ordering::SeqCst);
            let label = machine.label.as_str();
            *locked(&self.check_counts)
                .entry(label.to_owned())
                .or_default() += 1;
            let authenticated = locked(&self.authenticated).iter().any(|done| done == label);
            if self.needs_authentication.contains(&label) && !authenticated {
                return Err(ssh_failure("user@host: Permission denied (publickey)."));
            }
            match label {
                "offline" => Err(io::Error::from(io::ErrorKind::TimedOut)),
                "hostkey" => Err(ssh_failure("Host key verification failed.")),
                "old" => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the machine runs another build",
                )),
                // A remote client/server pair of two builds: discovery rejects it.
                "pair" => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the installed shepr-server is another build than shepr",
                )),
                "stale" | "authstale" => {
                    if *locked(&self.ready_now) {
                        return Ok(MachineSshCheck::Ready);
                    }
                    let boot = locked(&self.current_boot);
                    Ok(MachineSshCheck::DifferentBuild(server(&boot)))
                }
                _ => Ok(MachineSshCheck::Ready),
            }
        }

        fn authenticate(&self, machine: &MachineConfig) -> io::Result<()> {
            enter(&self.prompts_active, &self.max_prompts_active);
            self.log(format!("prompt {}", machine.label));
            std::thread::sleep(Duration::from_millis(20));
            self.prompts_active.fetch_sub(1, Ordering::SeqCst);
            if self
                .failing_authentication
                .contains(&machine.label.as_str())
            {
                Err(io::Error::other("ssh exited with exit status: 255"))
            } else {
                locked(&self.authenticated).push(machine.label.to_string());
                Ok(())
            }
        }

        fn stop_server(
            &self,
            machine: &MachineConfig,
            server: &DifferentBuildServer,
        ) -> io::Result<RemoteStop> {
            self.log(format!("stop {} {}", machine.label, server.boot_id));
            let script = locked(&self.stops)
                .pop_front()
                .ok_or_else(|| io::Error::other("no scripted stop"))?;
            match script {
                StopScript::Stopped => Ok(RemoteStop::Stopped),
                StopScript::ChangedTo(boot) => {
                    *locked(&self.current_boot) = boot.into();
                    Ok(RemoteStop::BootChanged)
                }
                StopScript::ChangedToReady => {
                    *locked(&self.ready_now) = true;
                    Ok(RemoteStop::BootChanged)
                }
                StopScript::Fails => Err(io::Error::other("remote server stop failed: timed out")),
            }
        }
    }

    #[test]
    fn every_machine_is_checked_at_once() {
        let machines = [machine("a"), machine("b"), machine("c"), machine("d")];
        let ssh = FakeSsh::new(&[]);
        let outcomes = preflight(&machines, &ssh, true, |_| {});
        assert_eq!(outcomes.len(), 4);
        assert_eq!(ssh.max_checks_active.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn prompts_run_one_at_a_time_in_configuration_order_after_the_checks() {
        let machines = [machine("a"), machine("ok"), machine("b"), machine("c")];
        let ssh = FakeSsh::new(&["a", "b", "c"]);
        let announced = Mutex::new(Vec::new());
        let outcomes = preflight(&machines, &ssh, true, |machine| {
            announced
                .lock()
                .expect("test precondition")
                .push(machine.label.to_string());
            // The announcement precedes the prompt it introduces.
            ssh.log(format!("announce {}", machine.label));
        });
        assert_eq!(ssh.max_prompts_active.load(Ordering::SeqCst), 1);
        assert_eq!(
            ssh.entries(),
            [
                "announce a",
                "prompt a",
                "announce b",
                "prompt b",
                "announce c",
                "prompt c"
            ]
        );
        assert_eq!(
            *announced.lock().expect("test precondition"),
            ["a", "b", "c"]
        );
        let prompted: Vec<_> = outcomes
            .iter()
            .map(|outcome| outcome.authentication.is_some())
            .collect();
        assert_eq!(prompted, [true, false, true, true]);
        assert!(
            outcomes
                .iter()
                .all(|outcome| { outcome.authentication.as_ref().is_none_or(Result::is_ok) })
        );
    }

    #[test]
    fn a_machine_is_checked_again_after_its_prompt_succeeded() {
        let machines = [machine("ok"), machine("a"), machine("authstale")];
        let ssh = FakeSsh::new(&["a", "authstale"]);
        let outcomes = preflight(&machines, &ssh, true, |_| {});
        // Two rounds: every machine, then the two that were prompted.
        assert_eq!(ssh.rounds.load(Ordering::SeqCst), 2);
        assert_eq!(ssh.checks_of("ok"), 1);
        assert_eq!(ssh.checks_of("a"), 2);
        assert_eq!(ssh.checks_of("authstale"), 2);
        assert!(matches!(outcomes[1].check, MachineCheck::Ready));
        // What the first check could not see is found by the second.
        assert!(
            matches!(&outcomes[2].check, MachineCheck::DifferentBuild(found) if found.boot_id == "boot-1")
        );
    }

    #[test]
    fn a_machine_whose_prompt_failed_is_not_checked_again() {
        let machines = [machine("a"), machine("ok")];
        let mut ssh = FakeSsh::new(&["a"]);
        ssh.failing_authentication = vec!["a"];
        let outcomes = preflight(&machines, &ssh, true, |_| {});
        assert_eq!(ssh.rounds.load(Ordering::SeqCst), 1);
        assert_eq!(ssh.checks_of("a"), 1);
        assert!(outcomes[0].check.needs_authentication());
    }

    #[test]
    fn only_authentication_failures_are_prompted_for() {
        let machines = [machine("offline"), machine("hostkey"), machine("old")];
        let ssh = FakeSsh::new(&[]);
        let outcomes = preflight(&machines, &ssh, true, |_| {
            panic!("no machine here needs a prompt");
        });
        assert!(ssh.entries().is_empty());
        assert!(matches!(outcomes[0].check, MachineCheck::Offline(_)));
        assert!(matches!(outcomes[1].check, MachineCheck::HostKey(_)));
        assert!(matches!(outcomes[2].check, MachineCheck::Incompatible(_)));
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.authentication.is_none())
        );
        assert_eq!(ssh.rounds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nothing_prompts_without_a_terminal() {
        let machines = [machine("a")];
        let ssh = FakeSsh::new(&["a"]);
        let outcomes = preflight(&machines, &ssh, false, |_| {
            panic!("no prompt without a terminal");
        });
        assert!(ssh.entries().is_empty());
        assert!(outcomes[0].check.needs_authentication());
        assert!(outcomes[0].authentication.is_none());
    }

    #[test]
    fn a_failed_prompt_is_reported_and_the_next_machine_is_still_prompted() {
        let machines = [machine("a"), machine("b")];
        let mut ssh = FakeSsh::new(&["a", "b"]);
        ssh.failing_authentication = vec!["a"];
        let outcomes = preflight(&machines, &ssh, true, |_| {});
        assert_eq!(ssh.entries(), ["prompt a", "prompt b"]);
        let failure = outcomes[0]
            .authentication
            .as_ref()
            .expect("a was prompted")
            .as_ref()
            .expect_err("a's ssh failed");
        assert!(failure.contains("255"), "{failure}");
        assert!(matches!(outcomes[1].authentication, Some(Ok(()))));
    }

    #[test]
    fn outcomes_keep_configuration_order_and_labels() {
        let machines = [machine("offline"), machine("ok"), machine("a")];
        let ssh = FakeSsh::new(&["a"]);
        let outcomes = preflight(&machines, &ssh, true, |_| {});
        let labels: Vec<_> = outcomes
            .iter()
            .map(|outcome| outcome.label.as_str())
            .collect();
        assert_eq!(labels, ["offline", "ok", "a"]);
    }

    #[test]
    fn checks_are_classified_by_what_went_wrong() {
        assert!(matches!(
            classify_check(Ok(MachineSshCheck::Ready)),
            MachineCheck::Ready
        ));
        for (error, expected) in [
            (
                ssh_failure("user@host: Permission denied (password)."),
                "authentication",
            ),
            (ssh_failure("Host key verification failed."), "host key"),
            (
                ssh_failure("ssh: connect to host h port 22: Connection refused"),
                "offline",
            ),
            (io::Error::from(io::ErrorKind::TimedOut), "offline"),
            (
                io::Error::new(io::ErrorKind::Unsupported, "matching Shepr is not ready"),
                "incompatible",
            ),
            (
                io::Error::new(io::ErrorKind::InvalidData, "build mismatch"),
                "incompatible",
            ),
            (io::Error::other("something else"), "failed"),
        ] {
            let class = match classify_check(Err(error)) {
                MachineCheck::Ready => "ready",
                MachineCheck::NeedsAuthentication(_) => "authentication",
                MachineCheck::Offline(_) => "offline",
                MachineCheck::HostKey(_) => "host key",
                MachineCheck::DifferentBuild(_) => "different build",
                MachineCheck::Incompatible(_) => "incompatible",
                MachineCheck::Failed(_) => "failed",
            };
            assert_eq!(class, expected);
        }
        assert!(matches!(
            classify_check(Ok(MachineSshCheck::DifferentBuild(server("boot-1")))),
            MachineCheck::DifferentBuild(_)
        ));
    }

    /// Runs the whole startup step for `machines` against `ssh`, answering every
    /// offer with the next of `answers` and recording what was asked.
    fn run_restarts(
        machines: &[MachineConfig],
        ssh: &FakeSsh,
        can_prompt: bool,
        answers: &[RestartDecision],
    ) -> (Vec<PreflightOutcome>, Vec<String>) {
        let mut outcomes = preflight(machines, ssh, can_prompt, |_| {});
        let mut answers = answers.iter().copied();
        let mut asked = Vec::new();
        restart_different_builds(
            machines,
            &mut outcomes,
            ssh,
            can_prompt,
            |machine, server| {
                asked.push(format!("{} {}", machine.label, server.boot_id));
                answers.next().expect("an answer for every offer")
            },
        );
        (outcomes, asked)
    }

    #[test]
    fn a_different_build_is_restarted_with_consent() {
        let machines = [machine("ok"), machine("stale")];
        let ssh = FakeSsh::new(&[]).with_stops([StopScript::Stopped]);
        let (outcomes, asked) = run_restarts(&machines, &ssh, true, &[RestartDecision::Restart]);
        // Only the machine running another build is asked, and the stop names
        // the boot that was observed.
        assert_eq!(asked, ["stale boot-1"]);
        assert_eq!(ssh.entries(), ["stop stale boot-1"]);
        assert_eq!(outcomes[0].restart, None);
        assert_eq!(outcomes[1].restart, Some(RestartResult::Stopped));
        assert!(matches!(outcomes[1].check, MachineCheck::Ready));
    }

    #[test]
    fn a_refused_offer_leaves_the_server_alone() {
        let machines = [machine("stale")];
        let ssh = FakeSsh::new(&[]);
        let (outcomes, asked) = run_restarts(&machines, &ssh, true, &[RestartDecision::Keep]);
        assert_eq!(asked, ["stale boot-1"]);
        assert!(ssh.entries().is_empty(), "nothing was stopped");
        assert_eq!(outcomes[0].restart, Some(RestartResult::Declined));
        assert!(matches!(outcomes[0].check, MachineCheck::DifferentBuild(_)));
    }

    #[test]
    fn without_a_terminal_nothing_is_asked_or_stopped() {
        let machines = [machine("stale")];
        let ssh = FakeSsh::new(&[]);
        let mut outcomes = preflight(&machines, &ssh, false, |_| {});
        restart_different_builds(&machines, &mut outcomes, &ssh, false, |_, _| {
            panic!("no offer without a terminal");
        });
        assert!(ssh.entries().is_empty(), "nothing was stopped");
        assert_eq!(outcomes[0].restart, Some(RestartResult::NoTerminal));
        assert!(matches!(outcomes[0].check, MachineCheck::DifferentBuild(_)));
    }

    #[test]
    fn a_changed_occupant_is_rediscovered_and_offered_again() {
        let machines = [machine("stale")];
        let ssh =
            FakeSsh::new(&[]).with_stops([StopScript::ChangedTo("boot-2"), StopScript::Stopped]);
        let (outcomes, asked) = run_restarts(
            &machines,
            &ssh,
            true,
            &[RestartDecision::Restart, RestartDecision::Restart],
        );
        // The second offer is for the instance the fresh check found, and the
        // second stop names that boot, not the first.
        assert_eq!(asked, ["stale boot-1", "stale boot-2"]);
        assert_eq!(ssh.entries(), ["stop stale boot-1", "stop stale boot-2"]);
        assert_eq!(outcomes[0].restart, Some(RestartResult::Stopped));
    }

    #[test]
    fn a_new_occupant_can_be_declined() {
        let machines = [machine("stale")];
        let ssh = FakeSsh::new(&[]).with_stops([StopScript::ChangedTo("boot-2")]);
        let (outcomes, asked) = run_restarts(
            &machines,
            &ssh,
            true,
            &[RestartDecision::Restart, RestartDecision::Keep],
        );
        assert_eq!(asked, ["stale boot-1", "stale boot-2"]);
        assert_eq!(ssh.entries(), ["stop stale boot-1"]);
        assert_eq!(outcomes[0].restart, Some(RestartResult::Declined));
        assert!(
            matches!(&outcomes[0].check, MachineCheck::DifferentBuild(now) if now.boot_id == "boot-2")
        );
    }

    #[test]
    fn a_changed_occupant_that_needs_no_restart_ends_the_offer() {
        let machines = [machine("stale")];
        let ssh = FakeSsh::new(&[]).with_stops([StopScript::ChangedToReady]);
        let (outcomes, asked) = run_restarts(&machines, &ssh, true, &[RestartDecision::Restart]);
        assert_eq!(asked, ["stale boot-1"]);
        assert_eq!(outcomes[0].restart, Some(RestartResult::OccupantChanged));
        assert!(matches!(outcomes[0].check, MachineCheck::Ready));
    }

    #[test]
    fn a_machine_that_keeps_changing_is_offered_only_a_bounded_number_of_times() {
        let machines = [machine("stale")];
        let ssh = FakeSsh::new(&[]).with_stops([
            StopScript::ChangedTo("boot-2"),
            StopScript::ChangedTo("boot-3"),
        ]);
        let (outcomes, asked) = run_restarts(
            &machines,
            &ssh,
            true,
            &[RestartDecision::Restart, RestartDecision::Restart],
        );
        assert_eq!(asked.len(), MAX_RESTART_OFFERS);
        assert_eq!(outcomes[0].restart, Some(RestartResult::OccupantChanged));
        assert!(
            matches!(&outcomes[0].check, MachineCheck::DifferentBuild(now) if now.boot_id == "boot-3")
        );
    }

    #[test]
    fn a_failed_stop_is_reported_and_the_check_still_says_different_build() {
        let machines = [machine("stale"), machine("stale2")];
        let ssh = FakeSsh::new(&[]).with_stops([StopScript::Fails]);
        let (outcomes, _) = run_restarts(&machines, &ssh, true, &[RestartDecision::Restart]);
        assert!(
            matches!(&outcomes[0].restart, Some(RestartResult::Failed(error)) if error.contains("timed out"))
        );
        assert!(matches!(outcomes[0].check, MachineCheck::DifferentBuild(_)));
        // The other machine is a plain ready one and is left out.
        assert_eq!(outcomes[1].restart, None);
    }

    #[test]
    fn an_authenticated_machine_running_another_build_is_offered_a_restart() {
        let machines = [machine("authstale")];
        let ssh = FakeSsh::new(&["authstale"]).with_stops([StopScript::Stopped]);
        let (outcomes, asked) = run_restarts(&machines, &ssh, true, &[RestartDecision::Restart]);
        assert_eq!(ssh.entries(), ["prompt authstale", "stop authstale boot-1"]);
        assert_eq!(asked, ["authstale boot-1"]);
        assert_eq!(outcomes[0].restart, Some(RestartResult::Stopped));
    }

    #[test]
    fn offline_and_unusable_machines_are_never_offered_a_restart() {
        let machines = [
            machine("offline"),
            machine("hostkey"),
            machine("old"),
            // A remote whose shepr and shepr-server are two builds.
            machine("pair"),
        ];
        let ssh = FakeSsh::new(&[]);
        let mut outcomes = preflight(&machines, &ssh, true, |_| {});
        restart_different_builds(&machines, &mut outcomes, &ssh, true, |_, _| {
            panic!("these machines cannot be restarted");
        });
        assert!(ssh.entries().is_empty());
        assert!(matches!(outcomes[0].check, MachineCheck::Offline(_)));
        assert!(matches!(outcomes[1].check, MachineCheck::HostKey(_)));
        assert!(matches!(outcomes[2].check, MachineCheck::Incompatible(_)));
        assert!(matches!(outcomes[3].check, MachineCheck::Incompatible(_)));
        assert!(outcomes.iter().all(|outcome| outcome.restart.is_none()));
    }
}
