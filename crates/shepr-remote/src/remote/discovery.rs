use super::*;

use crate::machine::RemoteExecutableError;
use std::io;
use std::process::Output;

pub(super) fn locate_remote_shepr(ssh: &RemoteSsh) -> io::Result<RemoteExecutable> {
    DiscoveryProgress::default().advance(&mut SshDiscovery {
        ssh,
        rejected_shell_unsafe_candidate: None,
    })
}

/// The SSH commands full discovery is made of, one method per remote round trip. Only
/// [`DiscoveryProgress`] sequences them; the seam exists so that sequencing, and resuming
/// it, can be tested without a remote host.
pub(super) trait DiscoverySteps {
    /// `command -v` through the remote login shell, which sets up the user's PATH.
    fn path_via_login_shell(&mut self) -> io::Result<Option<RemoteExecutable>>;
    /// `command -v` through `/bin/sh`, for login shells (xonsh) that reject it.
    fn path_via_sh(&mut self) -> io::Result<Option<RemoteExecutable>>;
    /// Executables found at the known install locations.
    fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>>;
    /// Whether `candidate` passes the caller's verification.
    fn matches(&mut self, candidate: &RemoteExecutable) -> io::Result<bool>;
    fn target(&self) -> &str;

    /// A path rejected because nested remote shell commands cannot safely use it.
    fn take_rejected_shell_unsafe_candidate(&mut self) -> Option<RejectedShellUnsafeCandidate> {
        None
    }
}

#[derive(Clone, Debug)]
pub(super) struct RejectedShellUnsafeCandidate {
    path: String,
    reason: RemoteExecutableError,
}

pub(super) struct SshDiscovery<'a> {
    ssh: &'a RemoteSsh,
    rejected_shell_unsafe_candidate: Option<RejectedShellUnsafeCandidate>,
}

impl DiscoverySteps for SshDiscovery<'_> {
    fn path_via_login_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
        let output = self
            .ssh
            .posix_user_shell_output(&format!("command -v {REMOTE_INSTALL_NAME}"))?;
        path_lookup_result_with_rejected_candidate(
            &output,
            &mut self.rejected_shell_unsafe_candidate,
        )
    }

    fn path_via_sh(&mut self) -> io::Result<Option<RemoteExecutable>> {
        let output = self
            .ssh
            .sh_output(&format!("command -v {REMOTE_INSTALL_NAME}\n"))?;
        path_lookup_result_with_rejected_candidate(
            &output,
            &mut self.rejected_shell_unsafe_candidate,
        )
    }

    fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>> {
        let output = self
            .ssh
            .sh_output(&known_remote_binary_candidate_script())?;
        if !output.status.success() {
            return Err(command_failed("remote binary discovery failed", &output));
        }
        Ok(
            remote_executables_from_path_discovery_with_rejected_candidate(
                &String::from_utf8_lossy(&output.stdout),
                &mut self.rejected_shell_unsafe_candidate,
            ),
        )
    }

    fn matches(&mut self, candidate: &RemoteExecutable) -> io::Result<bool> {
        let Some(status) = remote_client_status(self.ssh, candidate)? else {
            return Ok(false);
        };
        // This command reports the candidate binary's identity. Reject a
        // different build here instead of accepting it and failing later
        // during the server bridge's build-identity preamble. The same probe
        // carries the sibling `shepr-server` the candidate would start, which
        // must be this build too: a stale or missing sibling is an install
        // error, never a reason to fall back to another server binary.
        ensure_remote_client_build(self.target(), &status)?;
        ensure_remote_sibling_build(self.target(), &status)?;
        Ok(true)
    }

    fn target(&self) -> &str {
        self.ssh.target()
    }

    fn take_rejected_shell_unsafe_candidate(&mut self) -> Option<RejectedShellUnsafeCandidate> {
        self.rejected_shell_unsafe_candidate.take()
    }
}

/// Reads a `command -v` result. A failed lookup means no remote executable on that PATH,
/// except when the typed failure says ssh itself exited 255: then nothing was learned
/// about the remote, and recording "not found" would be wrong. A path rejected for
/// needing shell quoting is recorded in `rejected_candidate`.
fn path_lookup_result_with_rejected_candidate(
    output: &Output,
    rejected_candidate: &mut Option<RejectedShellUnsafeCandidate>,
) -> io::Result<Option<RemoteExecutable>> {
    if !output.status.success() {
        let error = command_failed("remote SSH connection failed", output);
        if super::SshFailureDiagnostic::from_error(&error).failed_before_remote_result() {
            return Err(error);
        }
        return Ok(None);
    }
    Ok(
        remote_executable_from_path_discovery_with_rejected_candidate(
            &String::from_utf8_lossy(&output.stdout),
            rejected_candidate,
        ),
    )
}

/// What full discovery of the remote executable has learned so far: the result of every
/// SSH round trip that already completed.
///
/// Discovery is several round trips (a login-shell `command -v`, a `/bin/sh` `command -v`
/// when that finds nothing, the known-locations script, then a status probe per candidate
/// until one matches), and without connection sharing each is a cold SSH connect. On a
/// slow enough link they do not all fit in one connection attempt's budget. A configured
/// machine's connector keeps its progress across attempts, so the next attempt resumes
/// with the first round trip that has not completed instead of starting over. Every
/// round trip is capped well below the attempt budget, so each attempt completes at
/// least one and discovery finishes after a bounded number of attempts, each of which
/// still ends within the budget.
///
/// Results are kept only when an attempt ended on a link failure (a timeout, the
/// attempt deadline, a dropped or refused connection): those say nothing about the
/// remote install. Any other error (a command that ran and failed, the not-ready
/// outcome, an ssh failure reported through a command's output) clears them, so the
/// next attempt rediscovers from scratch rather than resuming from facts that error
/// may have made stale.
#[derive(Default)]
pub(crate) struct DiscoveryProgress {
    login_shell_path: Option<Option<RemoteExecutable>>,
    sh_path: Option<Option<RemoteExecutable>>,
    /// Every candidate in probe order, once the known-locations script has run.
    candidates: Option<Vec<RemoteExecutable>>,
    /// How many of `candidates` were probed and did not match.
    probed: usize,
    /// Why the first rejected candidate was rejected: its status probe ran and
    /// failed, or it reported an incompatible build. Probing goes on in case a
    /// later path is the install this client can use.
    first_candidate_rejection: Option<io::Error>,
    rejected_shell_unsafe_candidate: Option<RejectedShellUnsafeCandidate>,
}

impl DiscoveryProgress {
    /// Runs the round trips not yet completed, in order, skipping candidates whose
    /// status probe failed or whose client or sibling build is incompatible. A link
    /// failure stops the pass. Returns the first matching candidate, the first
    /// candidate's rejection if none match, or the not-ready error when no candidate
    /// was there to reject.
    /// Progress survives only an error that is a link failure (`is_ssh_link_failure`,
    /// which includes running out of time); any other error clears it.
    pub(super) fn advance(
        &mut self,
        steps: &mut impl DiscoverySteps,
    ) -> io::Result<RemoteExecutable> {
        let result = self.run_remaining(steps);
        if let Err(error) = &result
            && !is_ssh_link_failure(error)
        {
            *self = Self::default();
        }
        result
    }

    pub(super) fn run_remaining(
        &mut self,
        steps: &mut impl DiscoverySteps,
    ) -> io::Result<RemoteExecutable> {
        if self.login_shell_path.is_none() {
            self.login_shell_path = Some(steps.path_via_login_shell()?);
            self.remember_rejected_candidate(steps);
        }
        let mut path_candidate = self.login_shell_path.clone().flatten();
        if path_candidate.is_none() {
            // Non-POSIX login shells such as xonsh reject `command -v`; retry through
            // /bin/sh while retaining the login-shell probe for shell-initialized PATHs.
            if self.sh_path.is_none() {
                self.sh_path = Some(steps.path_via_sh()?);
                self.remember_rejected_candidate(steps);
            }
            path_candidate = self.sh_path.clone().flatten();
        }
        if self.candidates.is_none() {
            let mut candidates = Vec::new();
            if let Some(candidate) = path_candidate {
                push_if_new_remote_binary_candidate(&mut candidates, candidate);
            }
            let known_locations = steps.known_locations()?;
            self.remember_rejected_candidate(steps);
            for candidate in known_locations {
                push_if_new_remote_binary_candidate(&mut candidates, candidate);
            }
            self.candidates = Some(candidates);
        }
        let candidates = self.candidates.clone().unwrap_or_default();
        while let Some(candidate) = candidates.get(self.probed) {
            match steps.matches(candidate) {
                Ok(true) => return Ok(candidate.clone()),
                Ok(false) => {}
                // A wrong build or a failing probe at one path does not rule out a
                // later candidate, such as the real binary behind a PATH shim.
                Err(error) if !is_ssh_link_failure(&error) => {
                    if self.first_candidate_rejection.is_none() {
                        self.first_candidate_rejection = Some(error);
                    }
                }
                Err(error) => return Err(error),
            }
            self.probed += 1;
        }
        if let Some(error) = self.first_candidate_rejection.take() {
            return Err(error);
        }
        let rejection = self
            .rejected_shell_unsafe_candidate
            .as_ref()
            .map_or_default(|candidate| {
                format!(
                    "; rejected executable path {:?}: {}",
                    candidate.path, candidate.reason
                )
            });
        Err(crate::remote_compatibility_error(format!(
            "matching Shepr is not ready on {}{rejection}; install or update it there manually and retry",
            steps.target(),
        )))
    }

    fn remember_rejected_candidate(&mut self, steps: &mut impl DiscoverySteps) {
        if let Some(candidate) = steps.take_rejected_shell_unsafe_candidate()
            && self.rejected_shell_unsafe_candidate.is_none()
        {
            self.rejected_shell_unsafe_candidate = Some(candidate);
        }
    }

    /// Whether any round trip has completed, so the next `advance` resumes mid-way.
    pub(crate) fn has_progress(&self) -> bool {
        self.login_shell_path.is_some()
    }
}

/// Whether `candidate` (a remembered executable) still passes discovery's
/// verification: it runs, is this build, and has this build's sibling server.
/// `Ok(false)` means it is gone; a wrong build is an error, as in discovery.
pub(super) fn verify_remote_shepr(
    ssh: &RemoteSsh,
    candidate: &RemoteExecutable,
) -> io::Result<bool> {
    SshDiscovery {
        ssh,
        rejected_shell_unsafe_candidate: None,
    }
    .matches(candidate)
}

/// Continue status-probe discovery, resuming from and recording into `progress`.
pub(crate) fn resume_installed_remote_shepr_discovery(
    ssh: &RemoteSsh,
    progress: &mut DiscoveryProgress,
) -> io::Result<RemoteExecutable> {
    progress.advance(&mut SshDiscovery {
        ssh,
        rejected_shell_unsafe_candidate: None,
    })
}

pub(super) fn push_if_new_remote_binary_candidate(
    candidates: &mut Vec<RemoteExecutable>,
    candidate: RemoteExecutable,
) {
    if !candidates
        .iter()
        .any(|existing| existing.as_str() == candidate.as_str())
    {
        candidates.push(candidate);
    }
}

/// Cargo's bin directory follows the default destination used by `brokkr install`;
/// the local bin path also covers manual installs. These are checked before falling
/// back to `command -v`, which misses them when a non-interactive SSH shell has a
/// minimal PATH.
pub(super) fn known_remote_binary_candidate_script() -> String {
    format!(
        r#"home=${{HOME:-}}
cargo_home=${{CARGO_HOME:-}}
if [ -z "$cargo_home" ] && [ -n "$home" ]; then
    cargo_home="$home/.cargo"
fi
emit() {{
    path=$1
    if [ -n "$path" ] && [ -x "$path" ]; then
        printf '%s\n' "$path"
    fi
}}
if [ -n "$cargo_home" ]; then
    emit "$cargo_home/bin/{REMOTE_INSTALL_NAME}"
fi
if [ -n "$home" ]; then
    emit "$home/.local/bin/{REMOTE_INSTALL_NAME}"
fi
"#
    )
}

fn remote_executables_from_path_discovery_with_rejected_candidate(
    stdout: &str,
    rejected_candidate: &mut Option<RejectedShellUnsafeCandidate>,
) -> Vec<RemoteExecutable> {
    stdout
        .lines()
        .filter_map(|path| {
            remote_executable_from_path_recording_rejection(path, rejected_candidate)
        })
        .collect()
}

fn remote_executable_from_path_discovery_with_rejected_candidate(
    stdout: &str,
    rejected_candidate: &mut Option<RejectedShellUnsafeCandidate>,
) -> Option<RemoteExecutable> {
    stdout
        .lines()
        .find_map(|path| remote_executable_from_path_recording_rejection(path, rejected_candidate))
}

/// Parses one discovered path. The first absolute path rejected because it needs
/// shell quoting is kept in `rejected_candidate`, so a failed discovery can say why
/// an installed binary was skipped.
fn remote_executable_from_path_recording_rejection(
    path: &str,
    rejected_candidate: &mut Option<RejectedShellUnsafeCandidate>,
) -> Option<RemoteExecutable> {
    let path = path.trim();
    match RemoteExecutable::parse(path.to_owned()) {
        Ok(executable) => Some(executable),
        Err(reason @ RemoteExecutableError::NeedsShellQuoting) => {
            if rejected_candidate.is_none() {
                *rejected_candidate = Some(RejectedShellUnsafeCandidate {
                    path: path.to_owned(),
                    reason,
                });
            }
            None
        }
        Err(_) => None,
    }
}

pub(super) fn remote_client_status(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<Option<shepr_api::schema::ClientStatusJson>> {
    // Keep a distinct result for the `test -x` leg: a vanished candidate is
    // ordinary discovery progress, but a started status command that fails is
    // a diagnostic the operator needs to see.
    // limits-exempt: a shell exit status chosen for the remote command contract, not a bound.
    const CANDIDATE_NOT_EXECUTABLE: i32 = 125;
    let status_command = remote_shepr.status_client_command();
    let command = format!(
        "test -x {} || exit {CANDIDATE_NOT_EXECUTABLE}; {status_command}",
        remote_shepr.quoted(),
    );
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        if output.status.code() == Some(CANDIDATE_NOT_EXECUTABLE) {
            return Ok(None);
        }
        let error = remote_client_status_failure(&output);
        return Err(error);
    }
    parse_client_status_json(&String::from_utf8_lossy(&output.stdout))
        .map(Some)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "remote status client command returned no valid client status JSON",
            )
        })
}

fn remote_client_status_failure(output: &Output) -> io::Error {
    let context = if output.status.code() == Some(crate::SSH_OWN_FAILURE_EXIT_CODE) {
        "remote SSH connection failed"
    } else {
        "remote client status probe failed"
    };
    command_failed(context, output)
}

pub(super) fn parse_client_status_json(
    status: &str,
) -> Option<shepr_api::schema::ClientStatusJson> {
    status
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<shepr_api::schema::ClientStatusJson>(line).ok())
        .find(|status| status.version.is_some() || status.build_id.is_some())
}

fn ensure_remote_client_build(
    target: &str,
    status: &shepr_api::schema::ClientStatusJson,
) -> io::Result<()> {
    if status
        .build_id
        .as_deref()
        .is_some_and(shepr_protocol::is_this_build)
    {
        Ok(())
    } else {
        Err(remote_compatibility_error(target, status))
    }
}

/// Requires the remote client's sibling server to be this build, so the pair
/// installed on the host is the pair this client can use. A candidate whose
/// status does not report a sibling at all is one that predates the report.
fn ensure_remote_sibling_build(
    target: &str,
    status: &shepr_api::schema::ClientStatusJson,
) -> io::Result<()> {
    let install_hint =
        "Install shepr and shepr-server together from the same build on the host and retry";
    let Some(sibling) = status.server.as_ref() else {
        return Err(remote_candidate_mismatch(format!(
            "remote Shepr installation error on {target}: shepr did not report a shepr-server beside it. {install_hint}"
        )));
    };
    if let Some(error) = sibling.error.as_deref() {
        let binary = sibling
            .binary
            .as_deref()
            .map(super::server_lifecycle::printable_remote_text)
            .map_or_else(String::new, |binary| format!(" ({binary})"));
        return Err(remote_candidate_mismatch(format!(
            "remote Shepr installation error on {target}: shepr-server{binary} is unusable: {}. {install_hint}",
            super::server_lifecycle::printable_remote_text(error)
        )));
    }
    if sibling
        .build_id
        .as_deref()
        .is_some_and(shepr_protocol::is_this_build)
    {
        return Ok(());
    }
    let version = super::server_lifecycle::printable_remote_value(sibling.version.as_deref());
    let build_id = super::server_lifecycle::printable_remote_value(sibling.build_id.as_deref());
    Err(remote_candidate_mismatch(format!(
        "remote Shepr installation error on {target}: the shepr-server beside shepr is version {version} build {build_id}; this client is version {} build {}. {install_hint}",
        shepr_protocol::build_version(),
        shepr_protocol::BUILD_ID
    )))
}

fn remote_compatibility_error(
    target: &str,
    status: &shepr_api::schema::ClientStatusJson,
) -> io::Error {
    let version = super::server_lifecycle::printable_remote_value(status.version.as_deref());
    let build_id = super::server_lifecycle::printable_remote_value(status.build_id.as_deref());
    let advice = if shepr_config::BuildProfile::current() == shepr_config::BuildProfile::Dev {
        "This is a dev client, which needs a dev build of shepr on the remote host; discovery only finds installed builds (normally release), so install a dev build there and retry"
    } else {
        "Install the same Shepr build on the host and retry"
    };
    remote_candidate_mismatch(format!(
        "remote Shepr compatibility error on {target}: found version {version} build {build_id}; this client is version {} build {}. {advice}",
        shepr_protocol::build_version(),
        shepr_protocol::BUILD_ID
    ))
}

fn remote_candidate_mismatch(message: String) -> io::Error {
    crate::remote_compatibility_error(message)
}

pub(super) fn is_remote_candidate_mismatch(error: &io::Error) -> bool {
    super::SshFailureDiagnostic::from_error(error).is_remote_compatibility()
}

#[cfg(test)]
pub(super) fn path_lookup_result(output: &Output) -> io::Result<Option<RemoteExecutable>> {
    let mut rejected_candidate = None;
    path_lookup_result_with_rejected_candidate(output, &mut rejected_candidate)
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod discovery_tests;

#[cfg(test)]
pub(super) fn remote_executables_from_path_discovery(stdout: &str) -> Vec<RemoteExecutable> {
    let mut rejected_candidate = None;
    remote_executables_from_path_discovery_with_rejected_candidate(stdout, &mut rejected_candidate)
}

#[cfg(test)]
pub(super) fn remote_executable_from_path_discovery(stdout: &str) -> Option<RemoteExecutable> {
    let mut rejected_candidate = None;
    remote_executable_from_path_discovery_with_rejected_candidate(stdout, &mut rejected_candidate)
}
