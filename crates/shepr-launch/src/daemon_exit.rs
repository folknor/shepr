//! How a server daemon that ended during startup is told apart by the client
//! that launched it.
//!
//! A serving daemon exits with one of these codes and prints the reason
//! on its stderr; the client reads the code to word its own failure. The
//! printed error stays authoritative: this only names a class, so a server
//! that predates a code, or a daemon killed by a signal, is simply
//! [`DaemonExit::Failed`].

use crate::process_status::ProcessStatus;

/// Exit status of a server that found another server already running: a live
/// listener on a socket, or the data directory lease held.
// limits-exempt: process exit status shared by the server and client executables.
pub const ALREADY_RUNNING_EXIT_CODE: i32 = ProcessStatus::AlreadyRunning as i32;

/// Exit status of a server whose configuration or paths were refused.
// limits-exempt: process exit status shared by the server and client executables.
pub const CONFIG_REFUSED_EXIT_CODE: i32 = ProcessStatus::ConfigRefused as i32;

/// Exit status of any other startup or runtime failure.
// limits-exempt: process exit status shared by the server and client executables.
pub const FAILED_EXIT_CODE: i32 = ProcessStatus::Failed as i32;

/// The class of a server daemon's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonExit {
    /// The server stopped cleanly.
    Clean,
    /// Another server already holds the socket or the data directory.
    AlreadyRunning,
    /// The configuration or the paths it resolves were refused.
    ConfigRefused,
    /// Any other failure, including a signal.
    Failed,
}

impl DaemonExit {
    /// The class of a process's exit code; `None` (ended by a signal) is
    /// [`DaemonExit::Failed`].
    pub fn from_code(code: Option<i32>) -> Self {
        match code {
            Some(code) if code == ProcessStatus::Success as i32 => Self::Clean,
            Some(ALREADY_RUNNING_EXIT_CODE) => Self::AlreadyRunning,
            Some(CONFIG_REFUSED_EXIT_CODE) => Self::ConfigRefused,
            Some(_) | None => Self::Failed,
        }
    }

    /// The exit code a server ends with for this class, shared with clients
    /// that classify a daemon which ends during startup.
    pub fn code(self) -> i32 {
        i32::from(self.process_status().code())
    }

    pub const fn process_status(self) -> ProcessStatus {
        match self {
            Self::Clean => ProcessStatus::Success,
            Self::AlreadyRunning => ProcessStatus::AlreadyRunning,
            Self::ConfigRefused => ProcessStatus::ConfigRefused,
            Self::Failed => ProcessStatus::Failed,
        }
    }

    /// A short phrase for a message about a server that ended this way while
    /// starting.
    pub fn describe_boot_end(self) -> &'static str {
        match self {
            Self::Clean => "exited before it was ready",
            Self::AlreadyRunning => "found another server already running",
            Self::ConfigRefused => "refused its configuration",
            Self::Failed => "failed to start",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_round_trips_through_its_code() {
        for class in [
            DaemonExit::Clean,
            DaemonExit::AlreadyRunning,
            DaemonExit::ConfigRefused,
            DaemonExit::Failed,
        ] {
            assert_eq!(DaemonExit::from_code(Some(class.code())), class);
        }
    }

    #[test]
    fn unknown_codes_and_signals_are_plain_failures() {
        assert_eq!(DaemonExit::from_code(Some(2)), DaemonExit::Failed);
        assert_eq!(DaemonExit::from_code(None), DaemonExit::Failed);
    }
}
