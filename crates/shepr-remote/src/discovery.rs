use std::io;
use std::process::Output;

use shepr_launch::EndpointFailure;
use shepr_launch::invocation::PROGRAM_NAME as REMOTE_INSTALL_NAME;

use crate::failure::{
    RemoteExit, SshExit, failure_evidence, remote_candidate_mismatch_error,
    remote_compatibility_error,
};
use crate::machine::{RemoteExecutable, RemoteExecutableError, SshTarget};
use crate::server_lifecycle::remote_display_value;
use crate::shell_command::PosixScript;
use crate::ssh::{RemoteSsh, command_failed};

/// The SSH commands full discovery is made of, one method per remote round trip.
/// [`DiscoveryProgress`] and `installed_remote_shepr_candidates` both order the
/// candidates through `ordered_candidates`; the seam lets sequencing and resuming
/// be tested without a remote host.
pub(crate) trait DiscoverySteps {
    /// `command -v` under `/bin/sh`, started by sshd's non-login account shell so
    /// its environment supplies the PATH. The known-path probe below covers the
    /// standard install directories independently.
    fn path_via_account_shell(&mut self) -> io::Result<Option<RemoteExecutable>>;
    /// Executables in `$CARGO_HOME/bin` (or `$HOME/.cargo/bin`) and `$HOME/.local/bin`.
    fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>>;
    /// Whether `candidate` passes the caller's verification.
    fn matches(&mut self, candidate: &RemoteExecutable) -> io::Result<bool>;
    fn target(&self) -> &SshTarget;

    /// A path rejected because nested remote shell commands cannot safely use it.
    fn take_rejected_shell_unsafe_candidate(&mut self) -> Option<RejectedShellUnsafeCandidate> {
        None
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RejectedShellUnsafeCandidate {
    path: String,
    reason: RemoteExecutableError,
}

struct SshDiscovery<'a> {
    ssh: &'a RemoteSsh,
    rejected_shell_unsafe_candidate: Option<RejectedShellUnsafeCandidate>,
}

impl DiscoverySteps for SshDiscovery<'_> {
    fn path_via_account_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
        let script = PosixScript::new(format!("command -v {REMOTE_INSTALL_NAME}"));
        let output = self.ssh.sh_output(&script)?;
        path_lookup_result_with_rejected_candidate(
            &output,
            &mut self.rejected_shell_unsafe_candidate,
            self.ssh.has_established_session(),
        )
    }

    fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>> {
        let script = PosixScript::new(known_remote_binary_candidate_script());
        let output = self.ssh.sh_output(&script)?;
        if !output.status.success() {
            return Err(command_failed(
                "remote binary discovery failed",
                &output,
                self.ssh.has_established_session(),
            ));
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

    fn target(&self) -> &SshTarget {
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
    established_session: bool,
) -> io::Result<Option<RemoteExecutable>> {
    if !output.status.success() {
        let error = command_failed("remote SSH connection failed", output, established_session);
        if !failure_evidence(&error).rejects_candidate() {
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
/// Discovery is several round trips (an account-shell environment `command -v`, the
/// known-locations script, then a status probe per candidate until one matches), and
/// the first command may need to establish the shared SSH master. On a slow link they do
/// not all fit in one connection attempt's budget. A configured machine's connector keeps
/// its progress across attempts, so the next attempt resumes with the first round trip
/// that has not completed instead of starting over. Every round trip is capped well below
/// the attempt budget; completed work is retained even when a later round trip fails.
///
/// Progress survives failures that produced no remote result while the target
/// remains trusted, including network losses, round-trip timeouts that may be
/// waiting for authentication, and authentication refusals. A host-key, local
/// SSH configuration, remote rejection or unrecognized ssh failure leaves the
/// target's identity in doubt and clears it, as does any remote command or
/// compatibility result, since the installation may have changed.
#[derive(Default)]
pub(crate) struct DiscoveryProgress {
    account_shell_path: Option<Option<RemoteExecutable>>,
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
    /// status probe failed or whose client or sibling build is incompatible. A failed
    /// round trip stops the pass. Returns the first matching candidate, the first
    /// candidate's rejection if none match, or the not-ready error when no candidate
    /// was there to reject.
    /// Progress survives failures that returned no remote result while the target
    /// remains trusted. Target trust failures and remote results end this snapshot.
    pub(crate) fn advance(
        &mut self,
        steps: &mut impl DiscoverySteps,
    ) -> io::Result<RemoteExecutable> {
        let result = self.run_remaining(steps);
        if let Err(error) = &result
            && !failure_evidence(error).preserves_discovery()
        {
            *self = Self::default();
        }
        result
    }

    fn run_remaining(&mut self, steps: &mut impl DiscoverySteps) -> io::Result<RemoteExecutable> {
        if self.account_shell_path.is_none() {
            self.account_shell_path = Some(steps.path_via_account_shell()?);
            self.remember_rejected_candidate(steps);
        }
        let path_candidate = self.account_shell_path.clone().flatten();
        if self.candidates.is_none() {
            let known_locations = steps.known_locations()?;
            self.remember_rejected_candidate(steps);
            let candidates = ordered_candidates(path_candidate, known_locations);
            self.candidates = Some(candidates);
        }
        let candidates = self.candidates.clone().unwrap_or_default();
        while let Some(candidate) = candidates.get(self.probed) {
            match steps.matches(candidate) {
                Ok(true) => return Ok(candidate.clone()),
                Ok(false) => {}
                // A wrong build or a failing probe at one path does not rule out a
                // later candidate, such as the real binary behind a PATH shim. A
                // failure before any remote result says nothing about this
                // candidate, so it ends the pass with progress kept.
                Err(error) if failure_evidence(&error).rejects_candidate() => {
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
        Err(remote_compatibility_error(
            shepr_launch::guidance::remote_install_not_ready(steps.target(), &rejection),
        ))
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
        self.account_shell_path.is_some()
    }
}

/// Whether `candidate` (a remembered executable) still passes discovery's
/// verification: it runs, is this build, and has this build's sibling server.
/// `Ok(false)` means it is gone; a wrong build is an error, as in discovery.
pub(crate) fn verify_remote_shepr(
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

/// Every `shepr` installed on the host, whatever its build, in discovery's
/// order: the account shell's PATH first, then the known install directories.
/// Discovery itself keeps only a candidate of this build; the CLI's `--all`
/// talks to whichever build is there.
pub(crate) fn installed_remote_shepr_candidates(
    ssh: &RemoteSsh,
) -> io::Result<Vec<RemoteExecutable>> {
    let mut steps = SshDiscovery {
        ssh,
        rejected_shell_unsafe_candidate: None,
    };
    let path = steps.path_via_account_shell()?;
    let known = steps.known_locations()?;
    Ok(ordered_candidates(path, known))
}

fn ordered_candidates(
    path: Option<RemoteExecutable>,
    known: Vec<RemoteExecutable>,
) -> Vec<RemoteExecutable> {
    let mut candidates = Vec::new();
    for candidate in path.into_iter().chain(known) {
        push_if_new_remote_binary_candidate(&mut candidates, candidate);
    }
    candidates
}

fn push_if_new_remote_binary_candidate(
    candidates: &mut Vec<RemoteExecutable>,
    candidate: RemoteExecutable,
) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

/// Cargo's bin directory follows the default destination used by `brokkr install`;
/// the local bin path also covers manual installs. These are checked after
/// `command -v`, which misses them when a non-interactive SSH shell has a
/// minimal PATH. Installs elsewhere on a login-profile-only PATH are not discovered.
pub(crate) fn known_remote_binary_candidate_script() -> String {
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

fn remote_client_status(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<Option<shepr_api::schema::ClientStatusJson>> {
    let command = candidate_command(remote_shepr, &remote_shepr.status_client_command());
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        if SshExit::from_code(output.status.code()) == SshExit::Remote(RemoteExit::CandidateMissing)
        {
            return Ok(None);
        }
        let error = remote_client_status_failure(&output, ssh.has_established_session());
        return Err(error);
    }
    parse_remote_client_status_json(&String::from_utf8_lossy(&output.stdout)).map(Some)
}

pub(crate) fn parse_remote_client_status_json(
    status: &str,
) -> io::Result<shepr_api::schema::ClientStatusJson> {
    parse_client_status_json(status).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            EndpointFailure::incompatible(
                "remote status client command returned no valid client status JSON",
            ),
        )
    })
}

fn remote_client_status_failure(output: &Output, established_session: bool) -> io::Error {
    let context = if SshExit::from_code(output.status.code()) == SshExit::SshFailed {
        "remote SSH connection failed"
    } else {
        "remote client status probe failed"
    };
    command_failed(context, output, established_session)
}

pub(crate) fn parse_client_status_json(
    status: &str,
) -> Option<shepr_api::schema::ClientStatusJson> {
    json_records::<shepr_api::schema::ClientStatusJson>(status)
        .find(|status| status.identity.is_some())
}

/// Status commands emit one JSON record; shell startup or exit noise may surround it.
pub(crate) fn last_json_record<T: serde::de::DeserializeOwned>(stdout: &str) -> Option<T> {
    json_records(stdout).next()
}

fn json_records<'a, T: serde::de::DeserializeOwned + 'a>(
    stdout: &'a str,
) -> impl Iterator<Item = T> + 'a {
    stdout
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str(line).ok())
}

/// A vanished executable is distinct from a status command that ran and failed.
pub(crate) fn candidate_command(
    executable: &RemoteExecutable,
    command: &PosixScript,
) -> PosixScript {
    let missing = RemoteExit::CandidateMissing.code();
    PosixScript::new(format!(
        "test -x {} || exit {missing}; {}",
        executable.shell_word(),
        command.as_str()
    ))
}

fn ensure_remote_client_build(
    target: &SshTarget,
    status: &shepr_api::schema::ClientStatusJson,
) -> io::Result<()> {
    if status
        .identity
        .as_ref()
        .is_some_and(|identity| identity.build_id.is_this_build())
    {
        Ok(())
    } else {
        Err(client_build_mismatch(target, status))
    }
}

/// Requires the remote client's sibling server to be this build, so the pair
/// installed on the host is the pair this client can use. A candidate whose
/// status does not report a sibling despite matching this build is an invalid installation report.
fn ensure_remote_sibling_build(
    target: &SshTarget,
    status: &shepr_api::schema::ClientStatusJson,
) -> io::Result<()> {
    use shepr_launch::guidance::{RemoteInstallationFailure, remote_sibling_mismatch};
    let cause = match status.server.as_ref() {
        None => RemoteInstallationFailure::MissingSibling,
        Some(sibling) => match &sibling.identity {
            Err(error) => RemoteInstallationFailure::UnusableSibling {
                binary: sibling.binary.as_deref(),
                error,
            },
            Ok(identity) if identity.build_id.is_this_build() => return Ok(()),
            Ok(identity) => RemoteInstallationFailure::DifferentSibling {
                version: &identity.version,
                build_id: identity.build_id,
            },
        },
    };
    Err(remote_candidate_mismatch(remote_sibling_mismatch(
        target, cause,
    )))
}

fn client_build_mismatch(
    target: &SshTarget,
    status: &shepr_api::schema::ClientStatusJson,
) -> io::Error {
    let identity = status.identity.as_ref();
    let version = remote_display_value(identity.map(|identity| identity.version.as_str()));
    let build_id = identity.map_or_else(
        || "unknown".into(),
        |identity| identity.build_id.to_string(),
    );
    remote_candidate_mismatch(shepr_launch::guidance::remote_client_mismatch(
        target, &version, &build_id,
    ))
}

fn remote_candidate_mismatch(message: String) -> io::Error {
    // Cache verification may discard this executable. General compatibility errors
    // such as a not-ready install or server mismatch do not prove its path is stale.
    remote_candidate_mismatch_error(message)
}

#[cfg(test)]
mod tests;
