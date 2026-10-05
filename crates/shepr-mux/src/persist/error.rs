use std::io;

/// Why a save job did not run. These failures cannot be repaired by retrying
/// on the same persister.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveRefusal {
    StoppedAfterPanic,
    Retired,
}

/// The outcome of a session save.
#[derive(Debug)]
pub enum SaveError {
    /// The live workspace tree could not be captured consistently. No save
    /// job was produced, so the previous session file is still authoritative.
    /// Retryable: the tree lives in memory, and a later mutation (closing the
    /// workspace, for one) can make the next capture consistent again.
    CaptureInconsistent {
        workspace: String,
        detail: &'static str,
    },
    /// The original session file could not be opened to make the recovery
    /// copy required before replacing it. The operator must fix access and
    /// restart this server before saves can resume.
    BlockedOnBackup(io::Error),
    /// The write failed before it was known to have been published.
    Io(io::Error),
    /// The layout was published but could not be confirmed durable.
    PublishedNotDurable(io::Error),
    /// The worker ended before it reported this job's result.
    Abandoned,
    /// The persister accepted no further jobs in its current state.
    Refused(SaveRefusal),
}

impl SaveError {
    /// Whether a later attempt on this persister can plausibly succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::CaptureInconsistent { .. } | Self::Io(_) | Self::PublishedNotDurable(_)
        )
    }

    /// Whether the saved source could not be opened for its required backup.
    pub fn is_blocked_on_backup(&self) -> bool {
        matches!(self, Self::BlockedOnBackup(_))
    }
}

impl From<io::Error> for SaveError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CaptureInconsistent { workspace, detail } => {
                write!(
                    f,
                    "cannot capture inconsistent workspace {workspace}: {detail}"
                )
            }
            Self::BlockedOnBackup(error) => {
                write!(
                    f,
                    "cannot open the existing session file for its required backup: {error}"
                )
            }
            Self::Io(error) => std::fmt::Display::fmt(error, f),
            Self::PublishedNotDurable(error) => {
                write!(f, "session was published but may not be durable: {error}")
            }
            Self::Abandoned => f.write_str("session persister ended before finishing the save"),
            Self::Refused(SaveRefusal::StoppedAfterPanic) => {
                f.write_str("session persister stopped after a save panicked; no further saves run")
            }
            Self::Refused(SaveRefusal::Retired) => {
                f.write_str("session persister has been retired")
            }
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BlockedOnBackup(error) | Self::Io(error) | Self::PublishedNotDurable(error) => {
                Some(error)
            }
            Self::CaptureInconsistent { .. } | Self::Abandoned | Self::Refused(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capture inconsistency wrote nothing and may clear with the next
    /// mutation, so the saver retries it; an unopenable backup source blocks
    /// saves until the operator fixes it, and a refusal ends them.
    #[test]
    fn capture_inconsistencies_are_retried_and_backup_blocks_are_not() {
        let capture = SaveError::CaptureInconsistent {
            workspace: "w1".into(),
            detail: "layout and pane records disagree",
        };
        assert!(capture.is_retryable());
        assert!(!capture.is_blocked_on_backup());

        let blocked = SaveError::BlockedOnBackup(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(!blocked.is_retryable());
        assert!(blocked.is_blocked_on_backup());

        assert!(!SaveError::Refused(SaveRefusal::Retired).is_retryable());
    }
}
