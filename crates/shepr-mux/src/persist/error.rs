use std::io;

/// Why a save job did not run. These failures cannot be repaired by retrying
/// on the same persister.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveRefusal {
    LeaseOnly,
    StoppedAfterPanic,
    Retired,
}

/// The outcome of a session save.
#[derive(Debug)]
pub enum SaveError {
    /// The write failed before it was known to have been published, or a
    /// follow-up operation such as writing history failed.
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
        matches!(self, Self::Io(_) | Self::PublishedNotDurable(_))
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
            Self::Io(error) => std::fmt::Display::fmt(error, f),
            Self::PublishedNotDurable(error) => {
                write!(f, "session was published but may not be durable: {error}")
            }
            Self::Abandoned => f.write_str("session persister ended before finishing the save"),
            Self::Refused(SaveRefusal::LeaseOnly) => f.write_str(
                "this session persister only holds the data directory lease; it runs no saves",
            ),
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
            Self::Io(error) | Self::PublishedNotDurable(error) => Some(error),
            Self::Abandoned | Self::Refused(_) => None,
        }
    }
}
