//! The SSH failure vocabulary: the decoded exit bytes, the diagnostic every SSH
//! failure travels as, and the constructors for the typed errors the rest of
//! the crate raises.

use shepr_launch::{EndpointFailure, FailureCause, SshFailureClass};

mod evidence;

pub(crate) use evidence::{FailureEvidence, failure_evidence};

/// OpenSSH exits with 255 when ssh itself fails (resolve, connect, host key,
/// authentication, a dropped link); any other code came from the remote command.
// limits-exempt: an exit status of the OpenSSH and remote-shell contract.
pub(crate) const SSH_OWN_FAILURE_EXIT_CODE: i32 = 255;
// limits-exempt: remote exit 255 is remapped to 254 to keep it apart from SSH failures.
pub(crate) const REMAPPED_REMOTE_255_EXIT_CODE: i32 = 254;

/// OpenSSH's process result, decoded before endpoint policy sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SshExit {
    SshFailed,
    Remote(RemoteExit),
    Signalled,
}

/// Remote exit bytes with meanings used by discovery and bridge launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteExit {
    CandidateMissing,
    NotExecutable,
    NotFound,
    /// The wrapper aliases native 254 and remote 255; neither can be inferred.
    Remapped255Or254,
    Code(i32),
}

/// The remote exits a code alone names, decoded through [`RemoteExit::code`],
/// which spells each code once.
const SPECIAL_REMOTE_EXITS: [RemoteExit; 3] = [
    RemoteExit::CandidateMissing,
    RemoteExit::NotExecutable,
    RemoteExit::NotFound,
];

impl SshExit {
    pub(crate) fn from_code(code: Option<i32>) -> Self {
        match code {
            Some(SSH_OWN_FAILURE_EXIT_CODE) => Self::SshFailed,
            Some(REMAPPED_REMOTE_255_EXIT_CODE) => Self::Remote(RemoteExit::Remapped255Or254),
            Some(code) => Self::Remote(
                SPECIAL_REMOTE_EXITS
                    .into_iter()
                    .find(|exit| exit.code() == code)
                    .unwrap_or(RemoteExit::Code(code)),
            ),
            None => Self::Signalled,
        }
    }
}

impl RemoteExit {
    /// The remote exit status this stands for: 125 is the probe script's
    /// own "candidate missing", 126 and 127 are the shell's.
    pub(crate) const fn code(self) -> i32 {
        match self {
            Self::CandidateMissing => 125,
            Self::NotExecutable => 126,
            Self::NotFound => 127,
            Self::Remapped255Or254 => REMAPPED_REMOTE_255_EXIT_CODE,
            Self::Code(code) => code,
        }
    }
}

/// An SSH-side failure: the neutral endpoint failure the client sees, plus
/// where in the SSH machinery it arose. The origin is what discovery reads
/// its evidence from and what tells a link failure from a remote command's
/// answer; it never leaves this crate. Text classification is reserved for
/// the SSH process boundary ([`Self::from_ssh_output`]).
///
/// Carried in an `io::Error`, the diagnostic names its endpoint failure as
/// its source, so `EndpointFailure::from_error` outside this crate reads the
/// neutral cause without knowing this type.
#[derive(Clone, Debug)]
pub(crate) struct SshFailureDiagnostic {
    failure: EndpointFailure,
    origin: SshFailureOrigin,
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
    /// The diagnostic an error carries, or one built from the endpoint
    /// failure it carries, or from its IO kind.
    pub(crate) fn from_error(error: &std::io::Error) -> Self {
        if let Some(diagnostic) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<Self>())
        {
            return diagnostic.clone();
        }
        let failure = EndpointFailure::from_error(error);
        let origin = match failure.cause() {
            FailureCause::Io(kind) => SshFailureOrigin::Io(kind),
            FailureCause::Incompatible | FailureCause::DifferentBuild => {
                SshFailureOrigin::RemoteCompatibility
            }
            FailureCause::LocalSetup | FailureCause::InvalidLocalSetup => {
                SshFailureOrigin::LocalSetup
            }
            FailureCause::Ssh(SshFailureClass::AuthenticationPending) => {
                SshFailureOrigin::CommandTimeout
            }
            FailureCause::Ssh(_) => SshFailureOrigin::SshOutput(SshExit::SshFailed),
            FailureCause::RemoteRepair
            | FailureCause::Backpressure
            | FailureCause::Retry
            | FailureCause::Unclassified
            | FailureCause::Shutdown(_)
            | FailureCause::NoServer
            | FailureCause::ServerStopping
            | FailureCause::ServerStarting => SshFailureOrigin::Message,
        };
        Self { failure, origin }
    }

    pub(crate) fn from_ssh_output(exit_code: Option<i32>, message: &str) -> Self {
        let exit = SshExit::from_code(exit_code);
        let failure = if exit == SshExit::SshFailed {
            EndpointFailure::ssh(classify_ssh_diagnostic(message), message)
        } else {
            EndpointFailure::unclassified(message)
        };
        Self {
            failure,
            origin: SshFailureOrigin::SshOutput(exit),
        }
    }

    /// A silent OpenSSH exit 255 after this transport has already returned a
    /// remote result. With `LogLevel=ERROR`, OpenSSH's keepalive-timeout log
    /// can be suppressed, leaving the dead connection with no diagnostic.
    /// Callers must use this only for an empty stderr and a known established
    /// session; first-connection failures still go through `from_ssh_output`.
    pub(crate) fn silent_established_session_link(message: &str) -> Self {
        Self {
            failure: EndpointFailure::ssh(SshFailureClass::Link, message),
            origin: SshFailureOrigin::SshOutput(SshExit::SshFailed),
        }
    }

    /// Adds display context while retaining this diagnostic's structured class.
    pub(crate) fn with_context(mut self, context: impl Into<String>) -> Self {
        self.failure = self.failure.with_context(&context.into());
        self
    }

    pub(crate) fn authentication_wait_timeout() -> Self {
        Self {
            failure: EndpointFailure::ssh(
                SshFailureClass::AuthenticationPending,
                "SSH command timed out before returning a remote result; interactive authentication may be needed",
            ),
            origin: SshFailureOrigin::CommandTimeout,
        }
    }

    pub(crate) fn ssh_class(&self) -> Option<SshFailureClass> {
        match self.failure.cause() {
            FailureCause::Ssh(class) => Some(class),
            _ => None,
        }
    }

    /// What this failure established about the remote executable. Display
    /// text is deliberately irrelevant: SSH output classification and typed
    /// remote status errors are the only sources of install evidence.
    fn evidence(&self) -> FailureEvidence {
        // The candidate guard's reserved exit and the shell's exec failures
        // all prove that the remembered executable could not run.
        if matches!(
            self.origin,
            SshFailureOrigin::SshOutput(SshExit::Remote(
                RemoteExit::CandidateMissing | RemoteExit::NotExecutable | RemoteExit::NotFound
            ))
        ) {
            return FailureEvidence::InstallStale;
        }
        match self.origin {
            SshFailureOrigin::RemoteCandidateMismatch => FailureEvidence::CandidateMismatch,
            SshFailureOrigin::RemoteCompatibility => FailureEvidence::InstallChanged,
            SshFailureOrigin::Io(kind) if shepr_launch::failure::is_link_error_kind(kind) => {
                FailureEvidence::NothingLearned
            }
            SshFailureOrigin::SshOutput(SshExit::SshFailed)
                if matches!(
                    self.ssh_class(),
                    Some(
                        SshFailureClass::HostKey
                            | SshFailureClass::Configuration
                            | SshFailureClass::RemoteRejected
                            | SshFailureClass::Unrecognized
                    )
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
}

impl std::fmt::Display for SshFailureDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.failure, formatter)
    }
}

impl std::error::Error for SshFailureDiagnostic {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.failure)
    }
}

fn classify_ssh_diagnostic(message: &str) -> SshFailureClass {
    // OpenSSH has no structured stderr format. Only the SSH-output constructor
    // uses these narrow signatures, and only for ssh's own exit 255; generic
    // errors and remote command output are classified by their typed source or
    // exit status.
    let message = message.to_ascii_lowercase();
    if message.contains("host key verification failed")
        || message.contains("remote host identification has changed")
        || message.contains("no matching host key")
    {
        return SshFailureClass::HostKey;
    }
    // sshd lists the methods that may continue, in its own order, so any
    // method list is a refusal. A Rust io error's "(os error N)" is not one.
    if (message.contains("permission denied (") && !message.contains("permission denied (os error"))
        || message.contains("too many authentication failures")
        || (message.contains("signing failed")
            && (message.contains("sign_and_send_pubkey") || message.contains("agent")))
    {
        return SshFailureClass::Authentication;
    }
    if is_local_ssh_configuration_error(&message) {
        return SshFailureClass::Configuration;
    }
    if message.contains("could not resolve hostname") {
        if message.contains("temporary failure in name resolution") {
            return SshFailureClass::Link;
        }
        return SshFailureClass::Configuration;
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
        return SshFailureClass::Link;
    }
    if message.contains("kex_exchange_identification:")
        && message.contains("connection closed by remote host")
    {
        return SshFailureClass::Link;
    }
    if message.contains("connection closed by ") || message.contains("received disconnect from ") {
        return SshFailureClass::RemoteRejected;
    }
    SshFailureClass::Unrecognized
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

/// Classifies platform policy while its typed error is still available.
/// IO transports carry endpoint policy, never an unclassified platform payload.
#[expect(
    clippy::needless_pass_by_value,
    reason = "a map_err adapter: the error is handed over by value"
)]
pub(crate) fn ssh_runtime_error(error: crate::ssh_paths::SshRuntimeError) -> std::io::Error {
    use crate::ssh_paths::SshRuntimeError;

    let kind = error.kind();
    let permanent = match &error {
        SshRuntimeError::UnsafeDirectory(_) => true,
        SshRuntimeError::Io(error) => error.kind() == std::io::ErrorKind::InvalidInput,
        SshRuntimeError::RandomSource(_) => false,
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
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        SshFailureDiagnostic {
            failure: EndpointFailure::incompatible(message),
            origin: SshFailureOrigin::RemoteCandidateMismatch,
        },
    )
}

/// A connection attempt that ran out of its time budget. `TimedOut`, so it counts as a link
/// failure (no rediscovery) and a transient one (a retry, not attention).
pub(crate) fn attempt_deadline_passed() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "SSH connection attempt ran out of time",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::{MachineCheck, classify_check};
    use shepr_launch::FailureDisposition;

    #[test]
    fn special_remote_exit_codes_round_trip() {
        for exit in SPECIAL_REMOTE_EXITS
            .into_iter()
            .chain([RemoteExit::Remapped255Or254])
        {
            assert_eq!(SshExit::from_code(Some(exit.code())), SshExit::Remote(exit));
        }
        assert_eq!(
            SshExit::from_code(Some(3)),
            SshExit::Remote(RemoteExit::Code(3))
        );
    }

    #[test]
    fn remote_host_key_error_matches_ssh_diagnostics() {
        for message in [
            "Host key verification failed.",
            "REMOTE HOST IDENTIFICATION HAS CHANGED!",
        ] {
            let failure =
                SshFailureDiagnostic::from_ssh_output(Some(SSH_OWN_FAILURE_EXIT_CODE), message);
            assert_eq!(
                failure.ssh_class(),
                Some(SshFailureClass::HostKey),
                "{message}"
            );
        }
        let error = std::io::Error::other("server closed connection");
        let unclassified = SshFailureDiagnostic::from_error(&error);
        assert_eq!(unclassified.ssh_class(), None);
    }

    #[test]
    fn remote_auth_error_matches_ssh_auth_denied() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "remote platform detection failed: user@host: Permission denied (publickey).",
        );
        assert_eq!(
            diagnostic.ssh_class(),
            Some(SshFailureClass::Authentication)
        );
    }

    #[test]
    fn remote_auth_error_matches_keyboard_interactive_denied() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "remote server status failed: user@host: Permission denied (keyboard-interactive).",
        );
        assert_eq!(
            diagnostic.ssh_class(),
            Some(SshFailureClass::Authentication)
        );
    }

    #[test]
    fn remote_auth_error_matches_refusals_for_any_offered_method() {
        for message in [
            "user@host: Permission denied (gssapi-with-mic).",
            "user@host: Permission denied (hostbased).",
            "user@host: Permission denied (gssapi-with-mic,password).",
            "user@host: Permission denied (",
        ] {
            let diagnostic =
                SshFailureDiagnostic::from_ssh_output(Some(SSH_OWN_FAILURE_EXIT_CODE), message);
            assert_eq!(
                diagnostic.ssh_class(),
                Some(SshFailureClass::Authentication),
                "{message}"
            );
            assert_eq!(
                diagnostic.failure.disposition(),
                FailureDisposition::Authentication,
                "{message}"
            );
        }
        let os_error = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "remote bridge failed: Permission denied (os error 13)",
        );
        assert_ne!(os_error.ssh_class(), Some(SshFailureClass::Authentication));
    }

    #[test]
    fn only_a_silent_failure_after_a_remote_result_is_a_link_loss() {
        let message = "remote SSH connection failed (exit status 255)";
        let first_connection =
            SshFailureDiagnostic::from_ssh_output(Some(SSH_OWN_FAILURE_EXIT_CODE), message);
        assert_eq!(
            first_connection.ssh_class(),
            Some(SshFailureClass::Unrecognized)
        );
        assert_eq!(
            first_connection.evidence(),
            FailureEvidence::TargetUntrusted
        );

        let established = SshFailureDiagnostic::silent_established_session_link(message);
        assert_eq!(established.ssh_class(), Some(SshFailureClass::Link));
        assert_eq!(established.evidence(), FailureEvidence::NothingLearned);
        assert_eq!(
            established.failure.disposition(),
            FailureDisposition::Offline
        );

        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "remote SSH connection failed: an unrecognized diagnostic",
        );
        assert_eq!(
            diagnostic.ssh_class(),
            Some(SshFailureClass::Unrecognized),
            "an established session does not make nonempty diagnostics disappear"
        );
    }

    #[test]
    fn remote_auth_error_ignores_non_auth_errors() {
        let error = std::io::Error::other("remote platform detection failed: unsupported platform");
        let diagnostic = SshFailureDiagnostic::from_error(&error);

        assert_eq!(diagnostic.ssh_class(), None);
    }

    #[test]
    fn remote_auth_error_matches_case_insensitive_signing_failures() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "SIGN_AND_SEND_PUBKEY: SIGNING FAILED for ED25519 from agent: agent refused operation",
        );
        assert_eq!(
            diagnostic.ssh_class(),
            Some(SshFailureClass::Authentication)
        );
    }

    #[test]
    fn remote_auth_error_does_not_treat_host_key_errors_as_authentication() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "Permission denied (publickey). Host key verification failed.",
        );
        assert_eq!(diagnostic.ssh_class(), Some(SshFailureClass::HostKey));
    }

    #[test]
    fn ssh_255_failures_separate_transient_network_from_actionable_diagnostics() {
        let cases = [
            (
                "ssh: connect to host h port 22: Connection refused",
                "offline",
                true,
                false,
                FailureEvidence::NothingLearned,
            ),
            (
                "kex_exchange_identification: Connection closed by remote host",
                "offline",
                true,
                false,
                FailureEvidence::NothingLearned,
            ),
            (
                "write: Broken pipe",
                "offline",
                true,
                false,
                FailureEvidence::NothingLearned,
            ),
            (
                "Received disconnect from h port 22:2: Too many authentication failures",
                "authentication",
                false,
                false,
                FailureEvidence::NothingLearned,
            ),
            (
                "ssh: Could not resolve hostname typo.example: Name or service not known",
                "failed",
                false,
                true,
                FailureEvidence::TargetUntrusted,
            ),
            (
                "/home/u/.ssh/config: line 12: Bad configuration option: hostkeyalgorithms",
                "failed",
                false,
                true,
                FailureEvidence::TargetUntrusted,
            ),
            (
                "Bad owner or permissions on /home/u/.ssh/config",
                "failed",
                false,
                true,
                FailureEvidence::TargetUntrusted,
            ),
            (
                "Connection closed by h port 22",
                "failed",
                false,
                false,
                FailureEvidence::TargetUntrusted,
            ),
            (
                "an unrecognized ssh error",
                "failed",
                false,
                false,
                FailureEvidence::TargetUntrusted,
            ),
        ];

        for (message, expected_class, transient, local_configuration, evidence) in cases {
            let diagnostic =
                SshFailureDiagnostic::from_ssh_output(Some(SSH_OWN_FAILURE_EXIT_CODE), message);
            assert!(matches!(
                diagnostic.origin,
                SshFailureOrigin::SshOutput(SshExit::SshFailed)
            ));
            assert_eq!(diagnostic.evidence(), evidence, "{message}");
            assert_eq!(
                diagnostic.failure.disposition() == FailureDisposition::Offline,
                transient,
                "{message}"
            );
            assert_eq!(
                diagnostic.ssh_class() == Some(SshFailureClass::Configuration),
                local_configuration,
                "{message}"
            );
            let class = match classify_check(Err(std::io::Error::other(diagnostic))) {
                MachineCheck::Ready => "ready",
                MachineCheck::NeedsAuthentication(_) => "authentication",
                MachineCheck::Offline(_) => "offline",
                MachineCheck::HostKey(_) => "host key",
                MachineCheck::Incompatible(_) => "incompatible",
                MachineCheck::Failed(_) => "failed",
            };
            assert_eq!(class, expected_class, "{message}");
        }
    }

    #[test]
    fn a_diagnostic_reaches_the_client_as_its_neutral_failure() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "Permission denied (publickey).",
        )
        .with_context("handshake failed");
        let error = std::io::Error::other(diagnostic);
        let failure = EndpointFailure::from_error(&error);
        assert_eq!(
            failure.cause(),
            FailureCause::Ssh(SshFailureClass::Authentication)
        );
        assert_eq!(failure.disposition(), FailureDisposition::Authentication);
        assert_eq!(
            failure.to_string(),
            "handshake failed: Permission denied (publickey)."
        );
        // The origin stays readable inside this crate.
        assert!(matches!(
            SshFailureDiagnostic::from_error(&error).origin,
            SshFailureOrigin::SshOutput(SshExit::SshFailed)
        ));
    }

    #[test]
    fn a_remote_command_failure_is_not_an_ssh_class() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(Some(127), "shepr: not found");
        assert_eq!(diagnostic.failure.cause(), FailureCause::Unclassified);
        assert!(matches!(
            diagnostic.origin,
            SshFailureOrigin::SshOutput(SshExit::Remote(RemoteExit::NotFound))
        ));
        assert_eq!(diagnostic.evidence(), FailureEvidence::InstallStale);
    }

    #[test]
    fn unreadable_remote_status_replies_have_the_same_operator_action() {
        for error in [
            crate::discovery::parse_remote_client_status_json("not JSON")
                .expect_err("malformed client status"),
            crate::server_lifecycle::parse_remote_server_status_json("not JSON")
                .expect_err("malformed server status"),
        ] {
            let failure = EndpointFailure::from_error(&error);
            assert_eq!(failure.disposition(), FailureDisposition::Incompatible);
            assert!(matches!(
                classify_check(Err(error)),
                MachineCheck::Incompatible(_)
            ));
        }
    }
}
