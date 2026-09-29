//! Startup authentication of configured machines.
//!
//! The TUI reaches machines with `BatchMode=yes`, so any prompt (password, key
//! passphrase, keyboard-interactive, FIDO touch) fails its connection. This
//! step runs once before the client takes over the terminal: it checks every
//! machine concurrently and without prompting, then walks the ones that need
//! authentication one at a time and runs interactive ssh on shepr's own
//! control socket for each. `ControlPersist` keeps the authenticated master
//! alive after that ssh exits, so the client's connectors reuse it.
//!
//! The orchestration talks to ssh only through [`PreflightSsh`], so its
//! parallelism, prompt serialization and classification are tested without a
//! host. This crate does not print: the caller announces each prompt through
//! the `before_authentication` callback and reports the returned outcomes.

use std::io;
use std::time::Instant;

use crate::SshFailureDiagnostic;
use crate::machine::{MachineConfig, MachineLabel};

/// The two SSH operations the preflight needs.
pub trait PreflightSsh: Sync {
    /// Checks one machine without prompting. Called for all machines at once,
    /// from separate threads.
    fn check(&self, machine: &MachineConfig) -> io::Result<()>;

    /// Runs interactive authentication for one machine in this terminal. Called
    /// for one machine at a time, from the calling thread.
    fn authenticate(&self, machine: &MachineConfig) -> io::Result<()>;
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
    /// The machine answered but cannot be served: no shepr, another build, or
    /// a running server that is not a detached daemon.
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
pub fn classify_check(result: io::Result<()>) -> MachineCheck {
    let error = match result {
        Ok(()) => return MachineCheck::Ready,
        Err(error) => error,
    };
    let diagnostic = SshFailureDiagnostic::from_error(&error);
    if diagnostic.requires_authentication() {
        MachineCheck::NeedsAuthentication(diagnostic)
    } else if diagnostic.is_host_key() {
        MachineCheck::HostKey(diagnostic)
    } else if diagnostic.is_link_failure() {
        MachineCheck::Offline(diagnostic)
    } else if diagnostic.needs_attention() {
        MachineCheck::Incompatible(diagnostic)
    } else {
        MachineCheck::Failed(diagnostic)
    }
}

/// How one machine came out of the preflight.
#[derive(Debug)]
pub struct PreflightOutcome {
    pub label: MachineLabel,
    /// The result of the non-interactive check, before any prompt.
    pub check: MachineCheck,
    /// `None` when no prompt was run for this machine; otherwise whether the
    /// interactive ssh succeeded, with its failure text.
    pub authentication: Option<Result<(), String>>,
}

/// Checks every machine concurrently, then authenticates the ones that need it
/// one after another, in configuration order. `before_authentication` runs just
/// before each prompt so the caller can say which machine it is for. With
/// `can_prompt` false no prompt runs at all, and machines needing
/// authentication are reported as such.
///
/// Outcomes come back in the order of `machines`.
pub fn preflight(
    machines: &[MachineConfig],
    ssh: &dyn PreflightSsh,
    can_prompt: bool,
    mut before_authentication: impl FnMut(&MachineConfig),
) -> Vec<PreflightOutcome> {
    let checks: Vec<MachineCheck> = std::thread::scope(|scope| {
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
    });

    machines
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
            }
        })
        .collect()
}

/// The real ssh behind [`PreflightSsh`]: [`check_saved_ssh`](crate::check_saved_ssh)
/// under one shared deadline, and `ssh_authentication_command` on shepr's
/// control socket.
pub struct SavedSshPreflight<'a> {
    paths: &'a shepr_config::AppPaths,
    settings: crate::SavedSshSettings,
    deadline: Instant,
}

impl<'a> SavedSshPreflight<'a> {
    /// The deadline for every check starts now.
    pub fn new(paths: &'a shepr_config::AppPaths, settings: crate::SavedSshSettings) -> Self {
        Self {
            paths,
            settings,
            // clock-io-ok: the deadline bounds real ssh IO for the whole check phase.
            deadline: Instant::now() + crate::limits::PREFLIGHT_CHECK_BUDGET,
        }
    }
}

impl PreflightSsh for SavedSshPreflight<'_> {
    fn check(&self, machine: &MachineConfig) -> io::Result<()> {
        crate::check_saved_ssh(
            self.paths,
            &machine.label,
            &machine.ssh,
            self.settings,
            self.deadline,
        )
    }

    fn authenticate(&self, machine: &MachineConfig) -> io::Result<()> {
        // The command's owner stays alive until the child has exited: OpenSSH
        // reads its temporary config after spawn.
        let mut authentication =
            crate::ssh_authentication_command(self.paths, &machine.ssh, self.settings)?;
        let status = authentication.command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("ssh exited with {status}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn machine(label: &str) -> MachineConfig {
        MachineConfig {
            label: MachineLabel::parse(label).expect("test precondition"),
            ssh: crate::SshTarget::parse(format!("{label}.example")).expect("test precondition"),
        }
    }

    fn ssh_failure(message: &str) -> io::Error {
        io::Error::other(SshFailureDiagnostic::from_ssh_output(
            Some(crate::SSH_OWN_FAILURE_EXIT_CODE),
            message.into(),
        ))
    }

    /// A scripted ssh: each machine's check result is looked up by label, and
    /// every call is logged with how many like calls were in flight.
    struct FakeSsh {
        needs_authentication: Vec<&'static str>,
        failing_authentication: Vec<&'static str>,
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
    }

    fn enter(active: &AtomicUsize, max: &AtomicUsize) {
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(now, Ordering::SeqCst);
    }

    impl PreflightSsh for FakeSsh {
        fn check(&self, machine: &MachineConfig) -> io::Result<()> {
            enter(&self.checks_active, &self.max_checks_active);
            // Long enough for every concurrent check to be in flight together.
            std::thread::sleep(Duration::from_millis(100));
            self.checks_active.fetch_sub(1, Ordering::SeqCst);
            let label = machine.label.as_str();
            if self.needs_authentication.contains(&label) {
                return Err(ssh_failure("user@host: Permission denied (publickey)."));
            }
            match label {
                "offline" => Err(io::Error::from(io::ErrorKind::TimedOut)),
                "hostkey" => Err(ssh_failure("Host key verification failed.")),
                "old" => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the machine runs another build",
                )),
                _ => Ok(()),
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
                Ok(())
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
                MachineCheck::Incompatible(_) => "incompatible",
                MachineCheck::Failed(_) => "failed",
            };
            assert_eq!(class, expected);
        }
    }
}
