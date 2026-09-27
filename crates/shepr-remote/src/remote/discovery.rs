use super::*;

use serde::Deserialize;
use std::io;
use std::process::Output;

pub(super) fn locate_remote_shepr(ssh: &RemoteSsh) -> io::Result<RemoteExecutable> {
    DiscoveryProgress::default().advance(&mut SshDiscovery {
        ssh,
        verification: CandidateVerification::StatusProbe,
    })
}

/// How a discovered candidate is confirmed. Both callers share candidate
/// enumeration (`DiscoverySteps` up to `known_locations`); only this differs.
#[derive(Clone, Copy)]
pub(super) enum CandidateVerification<'a> {
    /// The saved-SSH connector: the candidate runs and answers `status client`.
    StatusProbe,
    /// The API bridge: the candidate supports machine API forwarding for this
    /// session (`remote-api-bridge --check`), even with the server down.
    ApiForwarding { session: &'a str },
}

/// The SSH commands full discovery is made of, one method per remote round trip. Only
/// [`DiscoveryProgress`] sequences them; the seam exists so that sequencing, and resuming
/// it, can be tested without a remote host.
pub(super) trait DiscoverySteps {
    /// `command -v shepr` through the remote login shell, which sets up the user's PATH.
    fn path_via_login_shell(&mut self) -> io::Result<Option<RemoteExecutable>>;
    /// `command -v shepr` through `/bin/sh`, for login shells (xonsh) that reject it.
    fn path_via_sh(&mut self) -> io::Result<Option<RemoteExecutable>>;
    /// Executables found at the known install locations.
    fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>>;
    /// Whether `candidate` passes the caller's verification.
    fn matches(&mut self, candidate: &RemoteExecutable) -> io::Result<bool>;
    fn target(&self) -> &str;
}

pub(super) struct SshDiscovery<'a> {
    ssh: &'a RemoteSsh,
    verification: CandidateVerification<'a>,
}

impl DiscoverySteps for SshDiscovery<'_> {
    fn path_via_login_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
        let output = self.ssh.posix_user_shell_output("command -v shepr")?;
        path_lookup_result(&output)
    }

    fn path_via_sh(&mut self) -> io::Result<Option<RemoteExecutable>> {
        let output = self.ssh.sh_output("command -v shepr\n")?;
        path_lookup_result(&output)
    }

    fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>> {
        let output = self
            .ssh
            .sh_output(&known_remote_binary_candidate_script())?;
        if !output.status.success() {
            return Err(command_failed("remote binary discovery failed", &output));
        }
        Ok(remote_executables_from_path_discovery(
            &String::from_utf8_lossy(&output.stdout),
        ))
    }

    fn matches(&mut self, candidate: &RemoteExecutable) -> io::Result<bool> {
        match self.verification {
            CandidateVerification::StatusProbe => {
                let Some(status) = remote_client_status(self.ssh, candidate)? else {
                    return Ok(false);
                };
                // This command reports the candidate binary's identity. Reject a
                // different build here instead of accepting it and failing later
                // during the server bridge's protocol preamble.
                let expected_version = shepr_protocol::build_version();
                if status.version.as_deref() == Some(expected_version.as_str())
                    && status.protocol == Some(shepr_protocol::PROTOCOL_VERSION)
                {
                    Ok(true)
                } else {
                    Err(remote_compatibility_error(self.target(), &status))
                }
            }
            CandidateVerification::ApiForwarding { session } => {
                remote_api_forwarding_supported(self.ssh, candidate, session)
            }
        }
    }

    fn target(&self) -> &str {
        self.ssh.target()
    }
}

/// Reads a `command -v shepr` result. A failed lookup means no `shepr` on that PATH,
/// except when the typed failure says ssh itself exited 255: then nothing was learned
/// about the remote, and recording "not found" would be wrong.
pub(super) fn path_lookup_result(output: &Output) -> io::Result<Option<RemoteExecutable>> {
    if !output.status.success() {
        let error = command_failed("remote SSH connection failed", output);
        if super::SshFailureDiagnostic::from_error(&error).is_link_failure() {
            return Err(error);
        }
        return Ok(None);
    }
    Ok(remote_executable_from_path_discovery(
        &String::from_utf8_lossy(&output.stdout),
    ))
}

/// What full discovery of the remote executable has learned so far: the result of every
/// SSH round trip that already completed.
///
/// Discovery is several round trips (a login-shell `command -v`, a `/bin/sh` `command -v`
/// when that finds nothing, the known-locations script, then a status probe per candidate
/// until one matches), and without connection sharing each is a cold SSH connect. On a
/// slow enough link they do not all fit in one saved-machine attempt's budget. A saved
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
}

impl DiscoveryProgress {
    /// Runs the round trips not yet completed, in order, stopping at the first error.
    /// Returns the first candidate that matches, or the not-ready error when none does.
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
        }
        let mut path_candidate = self.login_shell_path.clone().flatten();
        if path_candidate.is_none() {
            // Non-POSIX login shells such as xonsh reject `command -v`; retry through
            // /bin/sh while retaining the login-shell probe for shell-initialized PATHs.
            if self.sh_path.is_none() {
                self.sh_path = Some(steps.path_via_sh()?);
            }
            path_candidate = self.sh_path.clone().flatten();
        }
        if self.candidates.is_none() {
            let mut candidates = Vec::new();
            if let Some(candidate) = path_candidate {
                push_if_new_remote_binary_candidate(&mut candidates, candidate);
            }
            for candidate in steps.known_locations()? {
                push_if_new_remote_binary_candidate(&mut candidates, candidate);
            }
            self.candidates = Some(candidates);
        }
        let candidates = self.candidates.clone().unwrap_or_default();
        while let Some(candidate) = candidates.get(self.probed) {
            if steps.matches(candidate)? {
                return Ok(candidate.clone());
            }
            self.probed += 1;
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "matching Shepr is not ready on {}; install or update it there manually and retry",
                steps.target(),
            ),
        ))
    }

    /// Whether any round trip has completed, so the next `advance` resumes mid-way.
    pub(crate) fn has_progress(&self) -> bool {
        self.login_shell_path.is_some()
    }
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod discovery_tests;

pub(super) fn prepare_remote_shepr(ssh: &RemoteSsh) -> io::Result<PreparedRemoteShepr> {
    Ok(PreparedRemoteShepr {
        remote_shepr: locate_remote_shepr(ssh)?,
    })
}

pub(super) fn find_installed_remote_shepr(ssh: &RemoteSsh) -> io::Result<RemoteExecutable> {
    locate_remote_shepr(ssh)
}

/// `find_installed_remote_shepr`, resuming from and recording into `progress`.
pub(crate) fn resume_installed_remote_shepr_discovery(
    ssh: &RemoteSsh,
    progress: &mut DiscoveryProgress,
) -> io::Result<RemoteExecutable> {
    progress.advance(&mut SshDiscovery {
        ssh,
        verification: CandidateVerification::StatusProbe,
    })
}

/// The executable the API bridge runs: the same candidates as the connector,
/// confirmed by the forwarding check instead of the status probe. The two
/// proofs stay separate because the metadata cache records only this one.
pub(crate) fn discover_remote_api_executable(
    ssh: &RemoteSsh,
    session: &str,
) -> io::Result<RemoteExecutable> {
    DiscoveryProgress::default().advance(&mut SshDiscovery {
        ssh,
        verification: CandidateVerification::ApiForwarding { session },
    })
}

pub(super) fn remote_api_forwarding_supported(
    ssh: &RemoteSsh,
    candidate: &RemoteExecutable,
    session: &str,
) -> io::Result<bool> {
    let output = ssh.sh_output(&candidate.api_bridge_check_command(session))?;
    if !output.status.success() {
        let error = command_failed("remote SSH connection failed", &output);
        if super::SshFailureDiagnostic::from_error(&error).is_link_failure() {
            return Err(error);
        }
        return Ok(false);
    }
    Ok(true)
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

/// Install locations checked before falling back to `command -v shepr`, which
/// misses these when a non-interactive SSH shell has a minimal PATH.
pub(super) fn known_remote_binary_candidate_script() -> String {
    String::from(
        r#"home=${HOME:-}
emit() {
    path=$1
    if [ -n "$path" ] && [ -x "$path" ]; then
        printf '%s\n' "$path"
    fi
}
if [ -n "$home" ]; then
    emit "$home/.cargo/bin/shepr"
    emit "$home/.local/bin/shepr"
fi
"#,
    )
}

pub(super) fn remote_executables_from_path_discovery(stdout: &str) -> Vec<RemoteExecutable> {
    stdout
        .lines()
        .filter_map(remote_executable_from_path)
        .collect()
}

pub(super) fn remote_executable_from_path_discovery(stdout: &str) -> Option<RemoteExecutable> {
    stdout.lines().find_map(remote_executable_from_path)
}

pub(super) fn remote_executable_from_path(path: &str) -> Option<RemoteExecutable> {
    let path = path.trim();
    RemoteExecutable::parse(path.to_owned()).ok()
}

pub(super) fn remote_client_status(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<Option<RemoteClientStatusJson>> {
    let output = ssh.sh_output(&remote_shepr.status_client_command())?;
    if !output.status.success() {
        let error = command_failed("remote SSH connection failed", &output);
        if super::SshFailureDiagnostic::from_error(&error).is_link_failure() {
            return Err(error);
        }
        return Ok(None);
    }
    Ok(parse_client_status_json(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[derive(Debug, Deserialize)]
pub(super) struct RemoteClientStatusJson {
    #[serde(default)]
    pub(super) version: Option<String>,
    #[serde(default)]
    pub(super) protocol: Option<u32>,
}

pub(super) fn parse_client_status_json(status: &str) -> Option<RemoteClientStatusJson> {
    status
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<RemoteClientStatusJson>(line).ok())
        .find(|status| status.version.is_some() || status.protocol.is_some())
}

fn remote_compatibility_error(target: &str, status: &RemoteClientStatusJson) -> io::Error {
    let version = status
        .version
        .as_deref()
        .filter(|version| version.chars().all(|ch| ch.is_ascii_graphic()))
        .unwrap_or("unknown");
    let protocol = status
        .protocol
        .map(|protocol| protocol.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "remote Shepr compatibility error on {target}: found version {version} and protocol {protocol}; this client requires version {} and protocol {}. Install the same Shepr build on the remote host and retry",
            shepr_protocol::build_version(),
            shepr_protocol::PROTOCOL_VERSION
        ),
    )
}
