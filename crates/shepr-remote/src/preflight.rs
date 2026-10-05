//! Startup authentication for configured machines.
//!
//! The TUI reaches machines with `BatchMode=yes`, so password and
//! keyboard-interactive prompts are disabled. A security-key agent can still
//! wait for user presence; when a full bounded SSH command times out before a
//! remote result, preflight offers foreground SSH for that candidate. This step
//! runs once before the client takes over the terminal: [`preflight`] checks
//! every machine concurrently and without prompting, walks the ones that need
//! authentication one at a time and runs interactive ssh on shepr's own control
//! socket for each, then checks those machines again, since the first check
//! could not see past the prompt. `ControlPersist` keeps the authenticated
//! master alive after that ssh exits, so the client's connectors reuse it.
//!
//! The check starts nothing on a machine, and a machine's server of another
//! build is not handled here: the client shows it with a Restart entry.
//!
//! The orchestration talks to ssh only through [`PreflightSsh`], so its
//! parallelism, prompt serialization and classification are tested without a
//! host. This crate does not print or read the terminal: the caller announces
//! each prompt through the `before_authentication` callback and reports the
//! returned outcomes.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use shepr_launch::{EndpointFailure, FailureCause, FailureDisposition, SshFailureClass};

use crate::machine::{MachineConfig, MachineLabel};
use crate::machine_ssh::{MachineProbe, MachineSshConnector};
use crate::ssh::ssh_authentication_command;

/// The SSH operations the preflight needs.
pub trait PreflightSsh: Sync {
    /// Called before each round of concurrent checks, so an implementation can
    /// give the round its own time budget: the re-check after a prompt starts
    /// long after the first check did.
    fn start_round(&self) {}

    /// Checks one machine without prompting and without starting anything.
    /// Called for all machines of a round at once, from separate threads.
    fn check(&self, machine: &MachineConfig) -> io::Result<()>;

    /// Runs interactive authentication for one machine in this terminal. Called
    /// for one machine at a time, from the calling thread.
    fn authenticate(&self, machine: &MachineConfig) -> Result<(), AuthenticationError>;
}

/// What the non-interactive check found out about one machine. A failed check
/// carries its neutral endpoint failure; presentation derives the operator
/// hints from its cause.
#[derive(Clone, Debug)]
pub enum MachineCheck {
    /// SSH works and a shepr of this build is installed there.
    Ready,
    /// Non-interactive SSH refused credentials or timed out before returning;
    /// foreground SSH may complete authentication or wait for key presence.
    NeedsAuthentication(EndpointFailure),
    /// The machine did not answer: timeout, refusal, no route.
    Offline(EndpointFailure),
    /// The host key is unknown or changed. Never accepted automatically.
    HostKey(EndpointFailure),
    /// The machine answered but cannot be served: no shepr, another build, a
    /// shepr-server beside it that is missing or another build, or a running
    /// server whose build or boot identity is unknown.
    Incompatible(EndpointFailure),
    /// Any other failure.
    Failed(EndpointFailure),
}

impl MachineCheck {
    pub fn needs_authentication(&self) -> bool {
        matches!(self, Self::NeedsAuthentication(_))
    }
}

/// Sorts a check result into the classes the preflight acts on.
pub fn classify_check(result: io::Result<()>) -> MachineCheck {
    let error = match result {
        Ok(()) => return MachineCheck::Ready,
        Err(error) => error,
    };
    let failure = EndpointFailure::from_error(&error);
    match failure.disposition() {
        FailureDisposition::Authentication | FailureDisposition::PossibleAuthentication => {
            MachineCheck::NeedsAuthentication(failure)
        }
        FailureDisposition::HostKey => MachineCheck::HostKey(failure),
        FailureDisposition::Offline => MachineCheck::Offline(failure),
        FailureDisposition::Incompatible => MachineCheck::Incompatible(failure),
        FailureDisposition::Repair | FailureDisposition::Retry => MachineCheck::Failed(failure),
    }
}

/// The foreground SSH attempt failed before it could authenticate.
#[derive(Debug)]
pub enum AuthenticationError {
    CouldNotRun(io::Error),
    Exited(std::process::ExitStatus),
}

impl From<io::Error> for AuthenticationError {
    fn from(error: io::Error) -> Self {
        Self::CouldNotRun(error)
    }
}

impl std::fmt::Display for AuthenticationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CouldNotRun(error) => write!(f, "could not run ssh: {error}"),
            Self::Exited(status) => write!(f, "ssh exited with {status}"),
        }
    }
}

impl std::error::Error for AuthenticationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CouldNotRun(error) => Some(error),
            Self::Exited(_) => None,
        }
    }
}

/// How one machine came out of the preflight.
#[derive(Debug)]
pub struct PreflightOutcome {
    /// The machine this outcome describes, so later steps do not have to pair
    /// it with a separate configuration slice by position.
    pub machine: MachineConfig,
    /// The latest non-interactive check: the one before any prompt, or the
    /// check that followed a successful prompt.
    pub check: MachineCheck,
    /// `None` when no prompt was run for this machine; otherwise whether the
    /// interactive ssh succeeded, with its process status or setup error.
    pub authentication: Option<Result<(), AuthenticationError>>,
}

/// Checks every machine concurrently, then authenticates the ones that need it
/// one after another, in configuration order, then checks those again.
/// `before_authentication` runs just before each prompt so the caller can say
/// which machine it is for. When it is absent, no prompt runs and machines
/// needing authentication are reported as such.
///
/// Each outcome owns its machine configuration. A machine whose prompt
/// succeeded carries the check that followed it; one whose prompt failed keeps
/// the check that asked for it.
pub fn preflight(
    machines: &[MachineConfig],
    ssh: &dyn PreflightSsh,
    mut before_authentication: Option<&mut dyn FnMut(&MachineConfig)>,
) -> Vec<PreflightOutcome> {
    let all: Vec<&MachineConfig> = machines.iter().collect();
    let checks = check_concurrently(ssh, &all);

    let mut outcomes: Vec<PreflightOutcome> = machines
        .iter()
        .zip(checks)
        .map(|(machine, check)| {
            let authentication = if check.needs_authentication() {
                if let Some(before_authentication) = before_authentication.as_deref_mut() {
                    before_authentication(machine);
                    Some(ssh.authenticate(machine))
                } else {
                    None
                }
            } else {
                None
            };
            PreflightOutcome {
                machine: machine.clone(),
                check,
                authentication,
            }
        })
        .collect();

    // These checks did not return a remote result. Look again after the
    // foreground attempt, now that the authenticated master is open.
    let authenticated: Vec<usize> = outcomes
        .iter()
        .enumerate()
        .filter(|(_, outcome)| matches!(outcome.authentication, Some(Ok(()))))
        .map(|(index, _)| index)
        .collect();
    if !authenticated.is_empty() {
        let rechecked: Vec<&MachineConfig> = authenticated
            .iter()
            .map(|&index| &outcomes[index].machine)
            .collect();
        let checks = check_concurrently(ssh, &rechecked);
        for (index, check) in authenticated.into_iter().zip(checks) {
            outcomes[index].check = check_after_authentication(check);
        }
    }
    outcomes
}

fn check_after_authentication(check: MachineCheck) -> MachineCheck {
    match check {
        MachineCheck::NeedsAuthentication(failure)
            if failure.cause() == FailureCause::Ssh(SshFailureClass::AuthenticationPending) =>
        {
            // Foreground authentication already succeeded. A later bounded
            // timeout is a failed check, not evidence that the remote refused it.
            MachineCheck::Failed(failure)
        }
        check => check,
    }
}

/// One round of checks, all machines at once.
fn check_concurrently(ssh: &dyn PreflightSsh, machines: &[&MachineConfig]) -> Vec<MachineCheck> {
    ssh.start_round();
    std::thread::scope(|scope| {
        let handles: Vec<_> = machines
            .iter()
            .map(|machine| scope.spawn(move || classify_check(ssh.check(machine))))
            .collect();
        let mut checks = Vec::with_capacity(handles.len());
        let mut panic_payload = None;
        for handle in handles {
            match handle.join() {
                Ok(check) => checks.push(check),
                Err(payload) => {
                    if panic_payload.is_none() {
                        panic_payload = Some(payload);
                    }
                }
            }
        }
        if let Some(payload) = panic_payload {
            // Finish joining all workers before unwinding through the caller.
            std::panic::resume_unwind(payload);
        }
        checks
    })
}

/// The real ssh behind [`PreflightSsh`]: one retained probe per configured machine,
/// under one shared deadline per round, and `ssh_authentication_command` on
/// shepr's control socket. [`Self::into_connectors`] hands each probe to the
/// client's connector.
pub struct MachineSshPreflight<'a> {
    paths: &'a shepr_paths::AppPaths,
    deadline: Mutex<Instant>,
    probes: Mutex<HashMap<MachineLabel, Arc<Mutex<MachineProbe>>>>,
}

impl<'a> MachineSshPreflight<'a> {
    /// The deadline for the first round of checks starts now.
    pub fn new(paths: &'a shepr_paths::AppPaths) -> Self {
        Self {
            paths,
            deadline: Mutex::new(round_deadline()),
            probes: Mutex::new(HashMap::new()),
        }
    }

    /// Transfers resolution progress and verified transports to the client.
    pub fn into_connectors(self, machines: &[MachineConfig]) -> Vec<MachineSshConnector> {
        let mut probes = self
            .probes
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        machines
            .iter()
            .map(|machine| {
                let probe = probes
                    .remove(&machine.label)
                    .map_or_else(Default::default, |probe| {
                        let mut probe = probe
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        std::mem::take(&mut *probe)
                    });
                MachineSshConnector::from_preflight(self.paths, machine, probe)
            })
            .collect()
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

    fn check(&self, machine: &MachineConfig) -> io::Result<()> {
        let deadline = *self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Hold the map only while finding this machine's state. Different machines
        // keep independent locks, so their SSH checks still run concurrently.
        let probe = Arc::clone(
            self.probes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(machine.label.clone())
                .or_default(),
        );
        let mut probe = probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        probe.check(self.paths, &machine.ssh, deadline)
    }

    fn authenticate(&self, machine: &MachineConfig) -> Result<(), AuthenticationError> {
        // The command's owner stays alive until the child has exited: OpenSSH
        // reads its temporary config after spawn.
        let mut authentication = ssh_authentication_command(self.paths, &machine.ssh)?;
        let status = authentication.command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(AuthenticationError::Exited(status))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::{
        SSH_OWN_FAILURE_EXIT_CODE, SshFailureDiagnostic, local_setup_error,
        remote_compatibility_error,
    };
    use crate::machine::{MachineLabel, SshTarget};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn machine(label: &str) -> MachineConfig {
        MachineConfig {
            label: MachineLabel::parse(label).expect("test precondition"),
            ssh: SshTarget::parse(format!("{label}.example")).expect("test precondition"),
            palette: shepr_config::DEFAULT_LOCAL_HUE,
        }
    }

    fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().expect("test precondition")
    }

    fn ssh_failure(message: &str) -> io::Error {
        io::Error::other(SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            message,
        ))
    }

    /// A scripted ssh: each machine's check result is looked up by label, and
    /// every call is logged with how many like calls were in flight.
    struct FakeSsh {
        needs_authentication: Vec<&'static str>,
        failing_authentication: Vec<&'static str>,
        authenticated: Mutex<Vec<String>>,
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
                check_counts: Mutex::new(HashMap::new()),
                rounds: AtomicUsize::new(0),
                checks_active: AtomicUsize::new(0),
                max_checks_active: AtomicUsize::new(0),
                prompts_active: AtomicUsize::new(0),
                max_prompts_active: AtomicUsize::new(0),
                log: Mutex::new(Vec::new()),
            }
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

        fn check(&self, machine: &MachineConfig) -> io::Result<()> {
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
                "pending" if !authenticated => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    SshFailureDiagnostic::authentication_wait_timeout(),
                )),
                "offline" => Err(io::Error::from(io::ErrorKind::TimedOut)),
                "hostkey" => Err(ssh_failure("Host key verification failed.")),
                "old" => Err(remote_compatibility_error("the machine runs another build")),
                _ => Ok(()),
            }
        }

        fn authenticate(&self, machine: &MachineConfig) -> Result<(), AuthenticationError> {
            enter(&self.prompts_active, &self.max_prompts_active);
            self.log(format!("prompt {}", machine.label));
            std::thread::sleep(Duration::from_millis(20));
            self.prompts_active.fetch_sub(1, Ordering::SeqCst);
            if self
                .failing_authentication
                .contains(&machine.label.as_str())
            {
                Err(AuthenticationError::Exited(
                    std::os::unix::process::ExitStatusExt::from_raw(255 << 8),
                ))
            } else {
                locked(&self.authenticated).push(machine.label.to_string());
                Ok(())
            }
        }
    }

    #[test]
    fn every_machine_is_checked_at_once() {
        let machines = [machine("a"), machine("b"), machine("c"), machine("d")];
        let ssh = FakeSsh::new(&[]);
        let outcomes = preflight(&machines, &ssh, Some(&mut |_| {}));
        assert_eq!(outcomes.len(), 4);
        assert_eq!(ssh.max_checks_active.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn prompts_run_one_at_a_time_in_configuration_order_after_the_checks() {
        let machines = [machine("a"), machine("ok"), machine("b"), machine("c")];
        let ssh = FakeSsh::new(&["a", "b", "c"]);
        let announced = Mutex::new(Vec::new());
        let outcomes = preflight(
            &machines,
            &ssh,
            Some(&mut |machine| {
                announced
                    .lock()
                    .expect("test precondition")
                    .push(machine.label.to_string());
                // The announcement precedes the prompt it introduces.
                ssh.log(format!("announce {}", machine.label));
            }),
        );
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
        let machines = [machine("ok"), machine("a"), machine("b")];
        let ssh = FakeSsh::new(&["a", "b"]);
        let outcomes = preflight(&machines, &ssh, Some(&mut |_| {}));
        // Two rounds: every machine, then the two that were prompted.
        assert_eq!(ssh.rounds.load(Ordering::SeqCst), 2);
        assert_eq!(ssh.checks_of("ok"), 1);
        assert_eq!(ssh.checks_of("a"), 2);
        assert_eq!(ssh.checks_of("b"), 2);
        // What the first check could not see past is found by the second.
        assert!(matches!(outcomes[1].check, MachineCheck::Ready));
        assert!(matches!(outcomes[2].check, MachineCheck::Ready));
    }

    #[test]
    fn a_machine_whose_prompt_failed_is_not_checked_again() {
        let machines = [machine("a"), machine("ok")];
        let mut ssh = FakeSsh::new(&["a"]);
        ssh.failing_authentication = vec!["a"];
        let outcomes = preflight(&machines, &ssh, Some(&mut |_| {}));
        assert_eq!(ssh.rounds.load(Ordering::SeqCst), 1);
        assert_eq!(ssh.checks_of("a"), 1);
        assert!(outcomes[0].check.needs_authentication());
    }

    #[test]
    fn only_authentication_failures_are_prompted_for() {
        let machines = [machine("offline"), machine("hostkey"), machine("old")];
        let ssh = FakeSsh::new(&[]);
        let outcomes = preflight(
            &machines,
            &ssh,
            Some(&mut |_| {
                panic!("no machine here needs a prompt");
            }),
        );
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
        let outcomes = preflight(&machines, &ssh, None);
        assert!(ssh.entries().is_empty());
        assert!(outcomes[0].check.needs_authentication());
        assert!(outcomes[0].authentication.is_none());
    }

    #[test]
    fn a_failed_prompt_is_reported_and_the_next_machine_is_still_prompted() {
        let machines = [machine("a"), machine("b")];
        let mut ssh = FakeSsh::new(&["a", "b"]);
        ssh.failing_authentication = vec!["a"];
        let outcomes = preflight(&machines, &ssh, Some(&mut |_| {}));
        assert_eq!(ssh.entries(), ["prompt a", "prompt b"]);
        let failure = outcomes[0]
            .authentication
            .as_ref()
            .expect("a was prompted")
            .as_ref()
            .expect_err("a's ssh failed");
        assert!(
            matches!(failure, AuthenticationError::Exited(status) if status.code() == Some(SSH_OWN_FAILURE_EXIT_CODE)),
            "{failure}"
        );
        assert!(matches!(outcomes[1].authentication, Some(Ok(()))));
    }

    #[test]
    fn outcomes_keep_configuration_order_and_labels() {
        let machines = [machine("offline"), machine("ok"), machine("a")];
        let ssh = FakeSsh::new(&["a"]);
        let outcomes = preflight(&machines, &ssh, Some(&mut |_| {}));
        let labels: Vec<_> = outcomes
            .iter()
            .map(|outcome| outcome.machine.label.as_str())
            .collect();
        assert_eq!(labels, ["offline", "ok", "a"]);
    }

    #[test]
    fn checks_are_classified_by_what_went_wrong() {
        assert!(matches!(classify_check(Ok(())), MachineCheck::Ready));
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
                remote_compatibility_error("matching Shepr is not ready"),
                "incompatible",
            ),
            (
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    SshFailureDiagnostic::authentication_wait_timeout(),
                ),
                "authentication",
            ),
            (io::Error::other("something else"), "failed"),
        ] {
            let class = match classify_check(Err(error)) {
                MachineCheck::Ready => "ready",
                MachineCheck::NeedsAuthentication(_) => "authentication",
                MachineCheck::Offline(_) => "offline",
                MachineCheck::HostKey(_) => "host key",
                MachineCheck::Incompatible(_) => "incompatible",
                MachineCheck::Failed(_) => "failed",
            };
            assert_eq!(class, expected);
        }
        for kind in [
            io::ErrorKind::InvalidInput,
            io::ErrorKind::InvalidData,
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Unsupported,
        ] {
            assert!(matches!(
                classify_check(Err(local_setup_error(
                    "local setup failure",
                    io::Error::new(kind, "the local operation failed"),
                ))),
                MachineCheck::Failed(_)
            ));
        }
    }

    #[test]
    fn a_bounded_ssh_timeout_gets_a_foreground_authentication_attempt() {
        let machines = [machine("pending")];
        let ssh = FakeSsh::new(&[]);
        let outcomes = preflight(&machines, &ssh, Some(&mut |_| {}));
        assert_eq!(ssh.entries(), ["prompt pending"]);
        assert_eq!(ssh.checks_of("pending"), 2);
        assert!(matches!(&outcomes[0].authentication, Some(Ok(()))));
        assert!(matches!(&outcomes[0].check, MachineCheck::Ready));
    }

    #[test]
    fn a_timeout_after_successful_authentication_is_not_reported_as_a_refusal() {
        let waiting = classify_check(Err(io::Error::new(
            io::ErrorKind::TimedOut,
            SshFailureDiagnostic::authentication_wait_timeout(),
        )));
        assert!(matches!(&waiting, MachineCheck::NeedsAuthentication(_)));
        assert!(matches!(
            check_after_authentication(waiting),
            MachineCheck::Failed(_)
        ));
    }
}
