//! The endpoint failure vocabulary, for every endpoint the client presents:
//! the local server and configured machines alike.
//!
//! A failure is built where its cause is known and keeps that cause as a
//! typed [`FailureCause`]; display text never decides policy. The one
//! disposition table, [`EndpointFailure::disposition`], turns the cause into
//! the operator action. An SSH failure is one cause among others, holding
//! only the class OpenSSH's own report was sorted into: the classifier, the
//! configured target and the discovery consequences of a failure belong to
//! the SSH crate, and the hints shown for a cause belong to [`crate::guidance`].

use std::io;

use crate::RemoteText;

/// The operator action established by a failure, independent of its display text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureDisposition {
    Retry,
    Offline,
    Authentication,
    PossibleAuthentication,
    HostKey,
    Incompatible,
    Repair,
}

impl FailureDisposition {
    pub fn needs_attention(self) -> bool {
        !matches!(
            self,
            Self::Retry | Self::Offline | Self::PossibleAuthentication
        )
    }

    pub fn client_action(self) -> &'static str {
        crate::guidance::failure_client_action(self)
    }
}

/// What an SSH failure was classified as: OpenSSH's report of its own failure
/// (its exit status 255), or a bounded SSH command that never returned a
/// remote result. A remote command's own failure is not an SSH failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshFailureClass {
    /// The remote refused every offered credential.
    Authentication,
    /// A bounded SSH command ended before returning, so foreground SSH may
    /// need to wait for interactive authentication or security-key presence.
    AuthenticationPending,
    /// The remote's host key is unknown or changed.
    HostKey,
    /// The remote was never reached or the link dropped: a retry can clear it.
    Link,
    /// ssh could not use the configured target or the local ssh configuration.
    Configuration,
    /// The remote answered and then closed or refused the connection.
    RemoteRejected,
    /// ssh failed with its own exit status for a reason shepr does not
    /// recognise. Reported rather than retried silently.
    Unrecognized,
}

/// Where a failure came from, established where it was known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureCause {
    /// An SSH failure of the given class.
    Ssh(SshFailureClass),
    /// A raw IO failure with nothing known beyond its kind.
    Io(io::ErrorKind),
    /// The remote end answered but is not a usable shepr of this build.
    Incompatible,
    /// A local setup operation failed; it may succeed on a later attempt.
    LocalSetup,
    /// A local setup operation failed in a way no retry can fix.
    InvalidLocalSetup,
    /// The remote host's shepr ran and refused to serve for a reason the
    /// operator must fix on that host, such as its own config.
    RemoteRepair,
    /// A local queue toward the endpoint filled.
    Backpressure,
    /// A transient condition the next attempt is expected to clear.
    Retry,
    /// A failure with no classification beyond its message, such as a remote
    /// command that exited with a failure status.
    Unclassified,
    /// The server said it is shutting down.
    Shutdown(shepr_protocol::ShutdownReason),
    /// No server runs on the endpoint's host, and the attempt was one that
    /// only attaches: nothing started one.
    NoServer,
    /// The endpoint's server is stopping and no longer accepts clients.
    ServerStopping,
    /// The endpoint's server is still restoring its session.
    ServerStarting,
    /// The endpoint's server is another shepr build than this client.
    DifferentBuild,
}

/// What a failure of the SSH bridge on a remote host asks of the operator, as
/// that host classified it. The host reports it as text, on its stderr, so it
/// has a token of its own; the client turns it back into an
/// [`EndpointFailure`] with [`RemoteFailureClass::endpoint_failure`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteFailureClass {
    /// Something on the remote host must be fixed: its installation, its
    /// environment, its configuration, or a server there that will not answer.
    Repair,
    /// A transient condition the next attempt is expected to clear, such as a
    /// server still starting.
    Retry,
    /// An attach-only bridge found no server on the host and started none.
    NoServer,
    /// An attach-only bridge found the host's server stopping.
    Stopping,
}

impl RemoteFailureClass {
    pub const ALL: [Self; 4] = [Self::Repair, Self::Retry, Self::NoServer, Self::Stopping];

    /// The class as the remote host writes it.
    pub fn token(self) -> &'static str {
        match self {
            Self::Repair => "repair",
            Self::Retry => "retry",
            Self::NoServer => "no-server",
            Self::Stopping => "stopping",
        }
    }

    /// The class a remote host wrote, or `None` for a token this build does
    /// not know.
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.token() == token)
    }

    /// The endpoint failure this class means for the client, with the remote
    /// host's diagnostic as its message.
    pub fn endpoint_failure(self, message: impl Into<String>) -> EndpointFailure {
        match self {
            Self::Repair => EndpointFailure::remote_repair(message),
            Self::Retry => EndpointFailure::retry(message),
            Self::NoServer => EndpointFailure::no_server(message),
            Self::Stopping => EndpointFailure::server_stopping(message),
        }
    }
}

/// A failure built where its cause is known, with terminal-safe display text.
#[derive(Clone, Debug)]
pub struct EndpointFailure {
    cause: FailureCause,
    message: RemoteText,
}

impl EndpointFailure {
    fn new(cause: FailureCause, message: &str) -> Self {
        Self {
            cause,
            message: RemoteText::from_untrusted(message),
        }
    }

    /// Raw IO boundary only. A typed failure is found in the error's payload,
    /// directly or as the source of a boundary-specific error that carries
    /// one, so it keeps its cause; anything else is classified by its kind.
    pub fn from_error(error: &io::Error) -> Self {
        if let Some(source) = error.get_ref() {
            let source: &(dyn std::error::Error + 'static) = source;
            if let Some(failure) = find_failure(source) {
                return failure.clone();
            }
        }
        Self::new(FailureCause::Io(error.kind()), &error.to_string())
    }

    pub fn ssh(class: SshFailureClass, message: impl Into<String>) -> Self {
        Self::new(FailureCause::Ssh(class), &message.into())
    }

    pub fn incompatible(message: impl Into<String>) -> Self {
        Self::new(FailureCause::Incompatible, &message.into())
    }

    pub fn local_setup(message: impl Into<String>) -> Self {
        Self::new(FailureCause::LocalSetup, &message.into())
    }

    pub fn fatal_local_setup(message: impl Into<String>) -> Self {
        Self::new(FailureCause::InvalidLocalSetup, &message.into())
    }

    pub fn remote_repair(message: impl Into<String>) -> Self {
        Self::new(FailureCause::RemoteRepair, &message.into())
    }

    pub fn backpressure(message: impl Into<String>) -> Self {
        Self::new(FailureCause::Backpressure, &message.into())
    }

    pub fn retry(message: impl Into<String>) -> Self {
        Self::new(FailureCause::Retry, &message.into())
    }

    pub fn unclassified(message: impl Into<String>) -> Self {
        Self::new(FailureCause::Unclassified, &message.into())
    }

    pub fn server_shutdown(reason: shepr_protocol::ShutdownReason) -> Self {
        Self::new(FailureCause::Shutdown(reason), &reason.to_string())
    }

    pub fn no_server(message: impl Into<String>) -> Self {
        Self::new(FailureCause::NoServer, &message.into())
    }

    pub fn server_stopping(message: impl Into<String>) -> Self {
        Self::new(FailureCause::ServerStopping, &message.into())
    }

    pub fn server_starting(message: impl Into<String>) -> Self {
        Self::new(FailureCause::ServerStarting, &message.into())
    }

    pub fn different_build(message: impl Into<String>) -> Self {
        Self::new(FailureCause::DifferentBuild, &message.into())
    }

    pub fn cause(&self) -> FailureCause {
        self.cause
    }

    /// Safe display text for a diagnostic card or other terminal output.
    pub fn message(&self) -> &RemoteText {
        &self.message
    }

    pub fn shutdown_reason(&self) -> Option<shepr_protocol::ShutdownReason> {
        match self.cause {
            FailureCause::Shutdown(reason) => Some(reason),
            _ => None,
        }
    }

    /// Whether the remote refused every offered credential, so only an
    /// interactive authentication can clear it.
    pub fn requires_authentication(&self) -> bool {
        self.cause == FailureCause::Ssh(SshFailureClass::Authentication)
    }

    /// Adds display context while retaining the cause.
    pub fn with_context(mut self, context: &str) -> Self {
        self.message = RemoteText::from_untrusted(&format!("{context}: {}", self.message));
        self
    }

    pub fn disposition(&self) -> FailureDisposition {
        use FailureDisposition as D;
        match self.cause {
            FailureCause::Ssh(class) => match class {
                SshFailureClass::Authentication => D::Authentication,
                SshFailureClass::AuthenticationPending => D::PossibleAuthentication,
                SshFailureClass::HostKey => D::HostKey,
                SshFailureClass::Link => D::Offline,
                SshFailureClass::Configuration
                | SshFailureClass::RemoteRejected
                | SshFailureClass::Unrecognized => D::Repair,
            },
            // Only a protocol boundary can establish incompatibility; bare IO
            // kinds also arise from local paths and socket setup.
            FailureCause::Incompatible | FailureCause::DifferentBuild => D::Incompatible,
            FailureCause::Io(kind) if is_link_error_kind(kind) => D::Offline,
            FailureCause::LocalSetup
            | FailureCause::InvalidLocalSetup
            | FailureCause::RemoteRepair => D::Repair,
            FailureCause::Io(_)
            | FailureCause::Backpressure
            | FailureCause::Retry
            | FailureCause::Unclassified
            | FailureCause::Shutdown(_)
            | FailureCause::NoServer
            | FailureCause::ServerStopping
            | FailureCause::ServerStarting => D::Retry,
        }
    }

    pub fn disconnect_notice(&self) -> &'static str {
        crate::guidance::disconnect_notice(self.cause(), self.disposition())
    }
}

impl std::fmt::Display for EndpointFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.message, f)
    }
}

impl std::error::Error for EndpointFailure {}

fn find_failure<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a EndpointFailure> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(failure) = error.downcast_ref::<EndpointFailure>() {
            return Some(failure);
        }
        current = error.source();
    }
    None
}

/// Whether an IO failure says the link to the endpoint could not be made, so
/// nothing was learned about the remote side and the endpoint reads as offline.
/// This deliberately does not use `shepr_platform::ipc::classify_stream_error`:
/// that classifier answers how a local stream ended, and a peer that went away
/// mid-session (a broken pipe, an unexpected EOF) is a remote fault to retry,
/// not evidence that the machine is unreachable.
pub fn is_link_error_kind(kind: io::ErrorKind) -> bool {
    // ConnectionReset during SSH discovery establishes no reachable endpoint;
    // on an established local stream it instead means that the peer left.
    // AddrInUse is left out: it describes a local bind collision, not remote
    // reachability.
    matches!(
        kind,
        io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::NetworkDown
    )
}

/// Whether retrying a local launch can recover from this IO failure.
/// This is a launch policy, separate from stream termination and SSH reachability:
/// a missing binary or permission failure needs repair rather than another start.
pub(crate) fn launch_io_allows_retry(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_retry_and_link_reachability_answer_different_questions() {
        assert!(launch_io_allows_retry(io::ErrorKind::BrokenPipe));
        assert!(!is_link_error_kind(io::ErrorKind::BrokenPipe));
        assert!(launch_io_allows_retry(io::ErrorKind::ConnectionReset));
        assert!(is_link_error_kind(io::ErrorKind::ConnectionReset));
        assert!(!launch_io_allows_retry(io::ErrorKind::NotFound));
        assert!(!launch_io_allows_retry(io::ErrorKind::PermissionDenied));
    }

    #[test]
    fn typed_causes_survive_io_kinds_and_display_context() {
        for (failure, disposition) in [
            (
                EndpointFailure::incompatible("bad reply"),
                FailureDisposition::Incompatible,
            ),
            (
                EndpointFailure::local_setup("reader could not start"),
                FailureDisposition::Repair,
            ),
            (
                EndpointFailure::backpressure("queue full"),
                FailureDisposition::Retry,
            ),
            (
                EndpointFailure::retry("server starting"),
                FailureDisposition::Retry,
            ),
        ] {
            // The envelope kind does not override what the boundary established.
            let error = io::Error::new(
                io::ErrorKind::InvalidData,
                failure.with_context("handshake failed"),
            );
            let restored = EndpointFailure::from_error(&error);
            assert_eq!(restored.disposition(), disposition);
            assert!(restored.to_string().starts_with("handshake failed:"));
        }
    }

    #[test]
    fn ssh_authentication_keeps_its_cause_after_wrapping() {
        let failure = EndpointFailure::ssh(
            SshFailureClass::Authentication,
            "Permission denied (publickey).",
        )
        .with_context("handshake failed");
        let error = io::Error::other(failure);
        let restored = EndpointFailure::from_error(&error);
        assert_eq!(restored.disposition(), FailureDisposition::Authentication);
        assert_eq!(
            restored.cause(),
            FailureCause::Ssh(SshFailureClass::Authentication)
        );
        assert!(restored.requires_authentication());
        assert_eq!(
            restored.to_string(),
            "handshake failed: Permission denied (publickey)."
        );
    }

    /// A boundary-specific error that names the failure as its source.
    #[derive(Debug)]
    struct Boundary(EndpointFailure);

    impl std::fmt::Display for Boundary {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }

    impl std::error::Error for Boundary {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn a_failure_carried_as_a_source_keeps_its_cause() {
        let error = io::Error::new(
            io::ErrorKind::ConnectionAborted,
            Boundary(EndpointFailure::ssh(
                SshFailureClass::HostKey,
                "Host key verification failed.",
            )),
        );
        let restored = EndpointFailure::from_error(&error);
        assert_eq!(restored.disposition(), FailureDisposition::HostKey);
        assert_eq!(restored.to_string(), "Host key verification failed.");
    }

    #[test]
    fn raw_io_failures_are_classified_by_their_kind() {
        for (kind, disposition) in [
            (io::ErrorKind::TimedOut, FailureDisposition::Offline),
            (
                io::ErrorKind::ConnectionRefused,
                FailureDisposition::Offline,
            ),
            (io::ErrorKind::InvalidData, FailureDisposition::Retry),
            (io::ErrorKind::Unsupported, FailureDisposition::Retry),
            (io::ErrorKind::AddrInUse, FailureDisposition::Retry),
            (io::ErrorKind::BrokenPipe, FailureDisposition::Retry),
        ] {
            let restored = EndpointFailure::from_error(&io::Error::from(kind));
            assert_eq!(restored.cause(), FailureCause::Io(kind));
            assert_eq!(restored.disposition(), disposition, "{kind:?}");
        }
    }

    #[test]
    fn every_ssh_class_has_its_operator_action() {
        for (class, disposition) in [
            (
                SshFailureClass::Authentication,
                FailureDisposition::Authentication,
            ),
            (
                SshFailureClass::AuthenticationPending,
                FailureDisposition::PossibleAuthentication,
            ),
            (SshFailureClass::HostKey, FailureDisposition::HostKey),
            (SshFailureClass::Link, FailureDisposition::Offline),
            (SshFailureClass::Configuration, FailureDisposition::Repair),
            (SshFailureClass::RemoteRejected, FailureDisposition::Repair),
            (SshFailureClass::Unrecognized, FailureDisposition::Repair),
        ] {
            assert_eq!(
                EndpointFailure::ssh(class, "ssh failed").disposition(),
                disposition,
                "{class:?}"
            );
        }
        assert_eq!(
            EndpointFailure::unclassified("remote command failed").disposition(),
            FailureDisposition::Retry
        );
    }

    #[test]
    fn every_remote_failure_class_round_trips_and_keeps_its_operator_action() {
        for (class, disposition, cause) in [
            (
                RemoteFailureClass::Repair,
                FailureDisposition::Repair,
                FailureCause::RemoteRepair,
            ),
            (
                RemoteFailureClass::Retry,
                FailureDisposition::Retry,
                FailureCause::Retry,
            ),
            (
                RemoteFailureClass::NoServer,
                FailureDisposition::Retry,
                FailureCause::NoServer,
            ),
            (
                RemoteFailureClass::Stopping,
                FailureDisposition::Retry,
                FailureCause::ServerStopping,
            ),
        ] {
            assert_eq!(RemoteFailureClass::from_token(class.token()), Some(class));
            let failure = class.endpoint_failure("remote detail");
            assert_eq!(failure.disposition(), disposition, "{class:?}");
            assert_eq!(failure.cause(), cause, "{class:?}");
            assert_eq!(failure.to_string(), "remote detail");
        }
        assert_eq!(
            EndpointFailure::different_build("another build").disposition(),
            FailureDisposition::Incompatible
        );
        assert_eq!(RemoteFailureClass::from_token("unknown"), None);
        assert_eq!(RemoteFailureClass::from_token(""), None);
    }

    #[test]
    fn shutdown_keeps_its_reason_and_queue_pressure_names_the_local_cause() {
        let failure = EndpointFailure::server_shutdown(shepr_protocol::ShutdownReason::Stopping);
        let restored = EndpointFailure::from_error(&io::Error::other(failure));
        assert_eq!(
            restored.shutdown_reason(),
            Some(shepr_protocol::ShutdownReason::Stopping)
        );
        assert_eq!(restored.to_string(), "server is shutting down");
        assert_eq!(restored.disposition(), FailureDisposition::Retry);
        assert_eq!(
            restored.disconnect_notice(),
            "server shut down; reconnecting"
        );
        assert_eq!(
            EndpointFailure::backpressure("queue full").disconnect_notice(),
            "local output queue filled; reconnecting"
        );
    }
}
