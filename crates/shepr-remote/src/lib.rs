mod failure;
use failure::FailureEvidence;
pub use failure::{EndpointFailure, FailureDisposition};
mod text;
pub use text::RemoteText;

mod limits;

#[path = "remote/args.rs"]
mod args;
#[path = "remote/bridge.rs"]
mod bridge;
#[path = "remote/discovery.rs"]
mod discovery;
#[path = "remote/host.rs"]
mod host;
#[path = "remote/launch.rs"]
mod launch;
#[path = "remote/local_server.rs"]
pub mod local_server;
pub mod machine;
#[path = "remote/machine_ssh.rs"]
mod machine_ssh;
#[path = "remote/preflight.rs"]
mod preflight;
#[path = "remote/process.rs"]
mod process;
#[path = "remote/server_lifecycle.rs"]
mod server_lifecycle;
#[path = "remote/shell_command.rs"]
mod shell_command;
#[path = "remote/ssh.rs"]
mod ssh;

use crate::machine::RemoteExecutable;
use bridge::*;
use discovery::*;
use launch::*;
use server_lifecycle::*;
use shell_command::*;
use ssh::*;

pub use crate::machine::SshTarget;
pub use args::*;
pub use bridge::{BridgeUpload, BridgeUploadEnd};
pub use host::run_remote_client_bridge;
pub use launch::{RemoteStop, shell_quote, stop_remote_server};
pub use machine_ssh::*;
pub use preflight::{
    AuthenticationError, MachineCheck, MachineSshPreflight, PreflightOutcome, PreflightSsh,
    RestartDecider, RestartDecision, RestartFailure, RestartResult, classify_check, preflight,
    restart_different_builds,
};
pub use server_lifecycle::{DifferentBuildServer, MachineSshCheck};
pub use ssh::{release_ssh_resources_before_exit, ssh_authentication_command};

/// OpenSSH's process result, decoded before endpoint policy sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshExit {
    SshFailed,
    Remote(RemoteExit),
    Signalled,
}

/// Remote exit bytes with meanings used by discovery and bridge launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteExit {
    CandidateMissing,
    NotExecutable,
    NotFound,
    /// The wrapper aliases native 254 and remote 255; neither can be inferred.
    Remapped255Or254,
    Code(i32),
}

impl SshExit {
    pub fn from_code(code: Option<i32>) -> Self {
        match code {
            Some(SSH_OWN_FAILURE_EXIT_CODE) => Self::SshFailed,
            Some(125) => Self::Remote(RemoteExit::CandidateMissing),
            Some(126) => Self::Remote(RemoteExit::NotExecutable),
            Some(127) => Self::Remote(RemoteExit::NotFound),
            Some(REMAPPED_REMOTE_255_EXIT_CODE) => Self::Remote(RemoteExit::Remapped255Or254),
            Some(code) => Self::Remote(RemoteExit::Code(code)),
            None => Self::Signalled,
        }
    }
}

impl RemoteExit {
    pub const fn code(self) -> i32 {
        match self {
            Self::CandidateMissing => 125,
            Self::NotExecutable => 126,
            Self::NotFound => 127,
            Self::Remapped255Or254 => REMAPPED_REMOTE_255_EXIT_CODE,
            Self::Code(code) => code,
        }
    }
}

/// SSH diagnostic classes. Endpoint operator policy lives in
/// `EndpointFailure::disposition`, including failures outside SSH.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SshFailure {
    /// The remote refused every offered credential.
    Authentication,
    /// A bounded SSH command ended before returning, so foreground SSH may
    /// need to wait for interactive authentication or security-key presence.
    AuthenticationPending,
    /// The remote's host key is unknown or changed.
    HostKey,
    /// A local SSH or endpoint setup operation failed.
    LocalSetup,
    /// The remote was never reached or the link dropped: a retry can clear it.
    Link,
    /// ssh could not use the configured target or the local ssh configuration.
    LocalConfiguration,
    /// The remote answered and then closed or refused the connection.
    RemoteRejected,
    /// ssh failed with its own exit status for a reason shepr does not
    /// recognise. Reported rather than retried silently.
    Unrecognized,
    /// The remote end is not a usable shepr of this build.
    Compatibility,
    /// Anything else: an IO error or a message with no ssh classification.
    Other,
}

/// An SSH diagnostic with its process cause kept alongside its text.
/// Text classification is reserved for the SSH process boundary. Generic
/// endpoint failures use `EndpointFailure`; this also adapts their diagnostics
/// for callers that only display text and SSH hints.
#[derive(Clone, Debug)]
pub struct SshFailureDiagnostic {
    failure: SshFailure,
    origin: SshFailureOrigin,
    message: RemoteText,
}

#[derive(Clone, Copy, Debug)]
enum SshFailureOrigin {
    Io(std::io::ErrorKind),
    SshOutput(SshExit),
    CommandTimeout,
    LocalSetup,
    RemoteCompatibility,
    RemoteCandidateMismatch,
    Message,
}

impl SshFailureDiagnostic {
    pub fn from_error(error: &std::io::Error) -> Self {
        EndpointFailure::from_error(error).diagnostic()
    }

    pub fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            failure: SshFailure::Other,
            origin: SshFailureOrigin::Message,
            message: RemoteText::from_untrusted(&message),
        }
    }

    pub fn from_ssh_output(exit_code: Option<i32>, message: &str) -> Self {
        let exit = SshExit::from_code(exit_code);
        let failure = if exit == SshExit::SshFailed {
            classify_ssh_diagnostic(message)
        } else {
            SshFailure::Other
        };
        Self {
            failure,
            origin: SshFailureOrigin::SshOutput(exit),
            message: RemoteText::from_untrusted(message),
        }
    }

    /// Adds display context while retaining this diagnostic's structured class.
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.message = RemoteText::from_untrusted(&format!("{}: {}", context.into(), self.message));
        self
    }

    /// Safe display text for a diagnostic card or other terminal output.
    pub fn text(&self) -> &RemoteText {
        &self.message
    }

    pub fn requires_authentication(&self) -> bool {
        self.failure == SshFailure::Authentication
    }

    pub(crate) fn is_authentication_wait_timeout(&self) -> bool {
        self.failure == SshFailure::AuthenticationPending
    }

    pub(crate) fn authentication_wait_timeout() -> Self {
        Self {
            failure: SshFailure::AuthenticationPending,
            origin: SshFailureOrigin::CommandTimeout,
            message: RemoteText::from_untrusted(
                "SSH command timed out before returning a remote result; interactive authentication may be needed",
            ),
        }
    }

    /// Classifies an error at a boundary that knows its source was local setup.
    /// The same `ErrorKind` values can describe remote failures, so callers must
    /// supply this context explicitly instead of relying on `from_error`.
    pub fn from_local_setup_error(error: &std::io::Error) -> Self {
        Self {
            failure: SshFailure::LocalSetup,
            origin: SshFailureOrigin::LocalSetup,
            message: RemoteText::from_untrusted(&error.to_string()),
        }
    }

    pub fn is_host_key(&self) -> bool {
        self.failure == SshFailure::HostKey
    }

    /// Whether the attempt failed before any remote command produced a result:
    /// SSH itself failed (whatever the cause, authentication and host key
    /// included), a bounded SSH command timed out, or a typed IO error says the
    /// link was never made or was lost. The bridge and the machine check use it
    /// so no such failure is read as a remote command's answer; discovery
    /// classifies failures through `EndpointFailure` evidence instead. It says
    /// nothing about whether a retry helps.
    pub fn failed_before_remote_result(&self) -> bool {
        match self.origin {
            SshFailureOrigin::Io(kind) => is_ssh_link_error_kind(kind),
            SshFailureOrigin::SshOutput(exit) => exit == SshExit::SshFailed,
            SshFailureOrigin::CommandTimeout => true,
            SshFailureOrigin::LocalSetup
            | SshFailureOrigin::RemoteCompatibility
            | SshFailureOrigin::RemoteCandidateMismatch
            | SshFailureOrigin::Message => false,
        }
    }

    fn evidence(&self) -> FailureEvidence {
        if matches!(
            self.origin,
            SshFailureOrigin::SshOutput(SshExit::Remote(
                RemoteExit::NotExecutable | RemoteExit::NotFound
            ))
        ) {
            return FailureEvidence::InstallStale;
        }
        match self.origin {
            SshFailureOrigin::RemoteCandidateMismatch => FailureEvidence::CandidateMismatch,
            SshFailureOrigin::RemoteCompatibility => FailureEvidence::InstallChanged,
            SshFailureOrigin::Io(kind) if is_ssh_link_error_kind(kind) => {
                FailureEvidence::NothingLearned
            }
            SshFailureOrigin::SshOutput(SshExit::SshFailed)
                if matches!(
                    self.failure,
                    SshFailure::HostKey
                        | SshFailure::LocalConfiguration
                        | SshFailure::RemoteRejected
                        | SshFailure::Unrecognized
                ) =>
            {
                FailureEvidence::TargetUntrusted
            }
            SshFailureOrigin::SshOutput(SshExit::SshFailed)
            | SshFailureOrigin::CommandTimeout
            | SshFailureOrigin::LocalSetup => FailureEvidence::NothingLearned,
            SshFailureOrigin::Io(_)
            | SshFailureOrigin::SshOutput(_)
            | SshFailureOrigin::Message => FailureEvidence::RemoteFault,
        }
    }

    /// Whether OpenSSH exited with its own failure status before returning a
    /// remote command result.
    pub fn is_ssh_process_failure(&self) -> bool {
        matches!(self.origin, SshFailureOrigin::SshOutput(SshExit::SshFailed))
    }

    /// The remote command's own nonzero exit status, when ssh ran the command
    /// and it failed (ssh's own exit 255 is not one).
    pub fn remote_exit_code(&self) -> Option<i32> {
        match self.origin {
            SshFailureOrigin::SshOutput(SshExit::Remote(exit)) => Some(exit.code()),
            SshFailureOrigin::Io(_)
            | SshFailureOrigin::SshOutput(_)
            | SshFailureOrigin::CommandTimeout
            | SshFailureOrigin::LocalSetup
            | SshFailureOrigin::RemoteCompatibility
            | SshFailureOrigin::RemoteCandidateMismatch
            | SshFailureOrigin::Message => None,
        }
    }

    /// Whether this failure is a transient connection problem that the client
    /// should retry without treating the machine as needing operator attention.
    pub fn is_transient_network_failure(&self) -> bool {
        self.failure == SshFailure::Link
    }

    /// Whether the operator needs to fix the configured SSH target or local
    /// OpenSSH configuration before this machine can be reached.
    pub fn needs_local_ssh_configuration(&self) -> bool {
        self.failure == SshFailure::LocalConfiguration
    }

    pub fn disposition(&self) -> FailureDisposition {
        EndpointFailure::from_ssh(self.clone()).disposition()
    }

    pub fn needs_attention(&self) -> bool {
        self.disposition().needs_attention()
    }
}

impl std::fmt::Display for SshFailureDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.message, formatter)
    }
}

impl std::error::Error for SshFailureDiagnostic {}

fn classify_ssh_diagnostic(message: &str) -> SshFailure {
    // OpenSSH has no structured stderr format. Only the SSH-output constructor
    // uses these narrow signatures, and only for ssh's own exit 255; generic
    // errors and remote command output are classified by their typed source or
    // exit status.
    let message = message.to_ascii_lowercase();
    if message.contains("host key verification failed")
        || message.contains("remote host identification has changed")
        || message.contains("no matching host key")
    {
        return SshFailure::HostKey;
    }
    if (message.contains("permission denied")
        && ["(publickey", "(keyboard-interactive", "(password"]
            .iter()
            .any(|method| message.contains(method)))
        || message.contains("too many authentication failures")
        || (message.contains("signing failed")
            && (message.contains("sign_and_send_pubkey") || message.contains("agent")))
    {
        return SshFailure::Authentication;
    }
    if is_local_ssh_configuration_error(&message) {
        return SshFailure::LocalConfiguration;
    }
    if message.contains("could not resolve hostname") {
        if message.contains("temporary failure in name resolution") {
            return SshFailure::Link;
        }
        return SshFailure::LocalConfiguration;
    }
    if [
        "connection timed out",
        "operation timed out",
        "connection refused",
        "no route to host",
        "network is unreachable",
        "network is down",
        "connection reset by peer",
        "broken pipe",
    ]
    .iter()
    .any(|signature| message.contains(signature))
    {
        return SshFailure::Link;
    }
    if message.contains("kex_exchange_identification:")
        && message.contains("connection closed by remote host")
    {
        return SshFailure::Link;
    }
    if message.contains("connection closed by ") || message.contains("received disconnect from ") {
        return SshFailure::RemoteRejected;
    }
    SshFailure::Unrecognized
}

fn is_local_ssh_configuration_error(message: &str) -> bool {
    [
        "bad configuration option:",
        "bad owner or permissions on ",
        "could not open user config file",
        "could not open config file",
        "missing argument for ",
        "extra arguments at end of line",
    ]
    .iter()
    .any(|signature| message.contains(signature))
}

/// Whether an IO failure says the link to the machine could not be made, so
/// nothing was learned about the remote side and the machine reads as offline.
/// This deliberately does not use `shepr_platform::ipc::classify_stream_error`:
/// that classifier answers how a local stream ended, and a peer that went away
/// mid-session (a broken pipe, an unexpected EOF) is a remote fault to retry,
/// not evidence that the machine is unreachable.
fn is_ssh_link_error_kind(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::AddrInUse
            | std::io::ErrorKind::HostUnreachable
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::NetworkDown
    )
}

/// Classifies platform policy while its typed error is still available.
/// IO transports carry endpoint policy, never an unclassified platform payload.
#[expect(
    clippy::needless_pass_by_value,
    reason = "a map_err adapter: the error is handed over by value"
)]
pub(crate) fn ssh_runtime_error(error: shepr_platform::SshRuntimeError) -> std::io::Error {
    let kind = error.kind();
    let permanent = match &error {
        shepr_platform::SshRuntimeError::UnsafeDirectory(_) => true,
        shepr_platform::SshRuntimeError::Io(error) => {
            error.kind() == std::io::ErrorKind::InvalidInput
        }
        shepr_platform::SshRuntimeError::RandomSource(_) => false,
    };
    let failure = if permanent {
        EndpointFailure::fatal_local_setup(error.to_string())
    } else {
        EndpointFailure::local_setup(error.to_string())
    };
    std::io::Error::new(kind, failure)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "a map_err adapter: the error is handed over by value"
)]
pub(crate) fn local_setup_error(context: &str, error: std::io::Error) -> std::io::Error {
    let failure = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<EndpointFailure>())
        .cloned()
        .unwrap_or_else(|| {
            if error.kind() == std::io::ErrorKind::InvalidInput {
                EndpointFailure::fatal_local_setup(error.to_string())
            } else {
                EndpointFailure::local_setup(error.to_string())
            }
        })
        .with_context(context);
    std::io::Error::new(error.kind(), failure)
}

pub(crate) fn remote_compatibility_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        EndpointFailure::incompatible(message),
    )
}

pub(crate) fn remote_candidate_mismatch_error(message: impl Into<String>) -> std::io::Error {
    let message = message.into();
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        SshFailureDiagnostic {
            failure: SshFailure::Compatibility,
            origin: SshFailureOrigin::RemoteCandidateMismatch,
            message: RemoteText::from_untrusted(&message),
        },
    )
}

/// Operator hint lines for a failed configured-machine SSH operation, one per
/// line and without a trailing newline. Empty when there is no hint. The
/// binary renders them; this crate does not print.
pub fn machine_ssh_error_hint(err: &SshFailureDiagnostic, target: &SshTarget) -> Vec<String> {
    if err.is_host_key() {
        vec![
            "hint: configured machines use strict host-key checking; add the host key to the configured known_hosts file, then retry."
                .to_string(),
        ]
    } else if err.needs_local_ssh_configuration() {
        vec![
            "hint: check the configured SSH target and local SSH configuration; OpenSSH reports the file and line for configuration errors."
                .to_string(),
        ]
    } else {
        remote_error_hint_for_failure(err, target)
    }
}

fn remote_error_hint_for_failure(
    failure: &SshFailureDiagnostic,
    target: &SshTarget,
) -> Vec<String> {
    if failure.requires_authentication() {
        vec![
            format!(
                "hint: verify SSH access first with `{}`.",
                ssh_check_command(target)
            ),
            "hint: if your SSH key has a passphrase, load it into ssh-agent with `ssh-add` before retrying."
                .to_string(),
        ]
    } else {
        Vec::new()
    }
}

fn ssh_check_command(target: &SshTarget) -> String {
    format!("ssh {}", target.shell_word())
}

#[cfg(test)]
impl SshFailureDiagnostic {
    pub(crate) fn is_remote_compatibility(&self) -> bool {
        self.disposition() == FailureDisposition::Incompatible
    }

    pub(crate) fn is_local_setup_failure(&self) -> bool {
        self.failure == SshFailure::LocalSetup
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_host() -> SshTarget {
        SshTarget::parse("host").expect("test precondition")
    }

    #[test]
    fn remote_host_key_error_matches_ssh_diagnostics() {
        let target = SshTarget::parse("host").expect("test precondition");
        for message in [
            "Host key verification failed.",
            "REMOTE HOST IDENTIFICATION HAS CHANGED!",
        ] {
            let failure =
                SshFailureDiagnostic::from_ssh_output(Some(SSH_OWN_FAILURE_EXIT_CODE), message);
            assert!(!machine_ssh_error_hint(&failure, &target).is_empty());
        }
        assert!(
            machine_ssh_error_hint(
                &SshFailureDiagnostic::from_message("server closed connection"),
                &target
            )
            .is_empty()
        );
    }

    #[test]
    fn remote_auth_error_matches_ssh_auth_denied() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "remote platform detection failed: user@host: Permission denied (publickey).",
        );
        assert!(!remote_error_hint_for_failure(&diagnostic, &test_host()).is_empty());
    }

    #[test]
    fn remote_auth_error_matches_keyboard_interactive_denied() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "remote server status failed: user@host: Permission denied (keyboard-interactive).",
        );
        assert!(!remote_error_hint_for_failure(&diagnostic, &test_host()).is_empty());
    }

    #[test]
    fn remote_auth_error_ignores_non_auth_errors() {
        let diagnostic = SshFailureDiagnostic::from_message(
            "remote platform detection failed: unsupported platform",
        );

        assert!(remote_error_hint_for_failure(&diagnostic, &test_host()).is_empty());
    }

    #[test]
    fn remote_auth_error_matches_case_insensitive_signing_failures() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "SIGN_AND_SEND_PUBKEY: SIGNING FAILED for ED25519 from agent: agent refused operation",
        );
        assert!(!remote_error_hint_for_failure(&diagnostic, &test_host()).is_empty());
    }

    #[test]
    fn remote_auth_error_does_not_treat_host_key_errors_as_authentication() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "Permission denied (publickey). Host key verification failed.",
        );
        assert!(remote_error_hint_for_failure(&diagnostic, &test_host()).is_empty());
    }

    #[test]
    fn ssh_255_failures_separate_transient_network_from_actionable_diagnostics() {
        let cases = [
            (
                "ssh: connect to host h port 22: Connection refused",
                "offline",
                true,
                false,
            ),
            (
                "kex_exchange_identification: Connection closed by remote host",
                "offline",
                true,
                false,
            ),
            ("write: Broken pipe", "offline", true, false),
            (
                "Received disconnect from h port 22:2: Too many authentication failures",
                "authentication",
                false,
                false,
            ),
            (
                "ssh: Could not resolve hostname typo.example: Name or service not known",
                "failed",
                false,
                true,
            ),
            (
                "/home/u/.ssh/config: line 12: Bad configuration option: hostkeyalgorithms",
                "failed",
                false,
                true,
            ),
            (
                "Bad owner or permissions on /home/u/.ssh/config",
                "failed",
                false,
                true,
            ),
            ("Connection closed by h port 22", "failed", false, false),
            ("an unrecognized ssh error", "failed", false, false),
        ];

        for (message, expected_class, transient, local_configuration) in cases {
            let diagnostic =
                SshFailureDiagnostic::from_ssh_output(Some(SSH_OWN_FAILURE_EXIT_CODE), message);
            assert!(diagnostic.is_ssh_process_failure(), "{message}");
            assert!(diagnostic.failed_before_remote_result(), "{message}");
            assert_eq!(
                diagnostic.is_transient_network_failure(),
                transient,
                "{message}"
            );
            assert_eq!(
                diagnostic.needs_local_ssh_configuration(),
                local_configuration,
                "{message}"
            );
            let class = match crate::classify_check(Err(std::io::Error::other(diagnostic))) {
                crate::MachineCheck::Ready => "ready",
                crate::MachineCheck::NeedsAuthentication(_) => "authentication",
                crate::MachineCheck::Offline(_) => "offline",
                crate::MachineCheck::HostKey(_) => "host key",
                crate::MachineCheck::DifferentBuild(_) => "different build",
                crate::MachineCheck::Incompatible(_) => "incompatible",
                crate::MachineCheck::Failed(_) => "failed",
            };
            assert_eq!(class, expected_class, "{message}");
        }
    }

    #[test]
    fn ssh_check_command_quotes_remote_target() {
        let target = SshTarget::parse("host name").expect("test precondition");
        assert_eq!(ssh_check_command(&target), "ssh 'host name'");
    }
}
