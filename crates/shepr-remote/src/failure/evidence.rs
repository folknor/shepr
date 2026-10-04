use std::io;

use super::SshFailureDiagnostic;

/// What a failure established about the remote executable.
/// This records evidence from the failure source; callers use the same result
/// when deciding whether to keep discovery progress or discard a cached path.
/// Several failures share one operator disposition but differ here, so this is
/// read from the failure's SSH origin, never from its disposition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureEvidence {
    /// No remote command result was observed, so existing knowledge is intact.
    NothingLearned,
    /// ssh failed in a way that leaves the target's identity in doubt: a host
    /// key, local configuration, remote rejection or unrecognized ssh failure.
    /// Previously discovered paths may belong to a different machine.
    /// Authentication refusals are not here: the host key was accepted.
    TargetUntrusted,
    /// The requested executable could not be run at its remembered path.
    InstallStale,
    /// A candidate ran but did not match the build or sibling pair.
    CandidateMismatch,
    /// The remote answered with an incompatible install or protocol result.
    InstallChanged,
    /// The remote answered, but the failure does not identify an install issue.
    RemoteFault,
}

impl FailureEvidence {
    pub(crate) fn preserves_discovery(self) -> bool {
        matches!(self, Self::NothingLearned)
    }

    pub(crate) fn invalidates_executable(self) -> bool {
        matches!(self, Self::InstallStale | Self::CandidateMismatch)
    }

    pub(crate) fn rejects_candidate(self) -> bool {
        !matches!(self, Self::NothingLearned | Self::TargetUntrusted)
    }
}

/// Interprets the source of a failure as evidence about remote discovery.
pub(crate) fn failure_evidence(error: &io::Error) -> FailureEvidence {
    SshFailureDiagnostic::from_error(error).evidence()
}
