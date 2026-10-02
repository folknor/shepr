use std::io;

use crate::{SshFailure, SshFailureDiagnostic, SshFailureOrigin, SshTarget};

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
        if self.needs_attention() {
            "The client shows it as unavailable and needs attention; it keeps retrying it."
        } else {
            "The client keeps retrying it."
        }
    }
}

#[derive(Clone, Debug)]
enum Cause {
    Ssh(SshFailureDiagnostic),
    Io(io::ErrorKind),
    Incompatible,
    LocalSetup,
    Backpressure,
    Retry,
    Shutdown(Option<shepr_protocol::ShutdownReason>),
}

/// A failure built where its cause is known. SSH stderr classification is one
/// possible cause, rather than the vocabulary for local and protocol failures.
#[derive(Clone, Debug)]
pub struct EndpointFailure {
    cause: Cause,
    message: String,
}

impl EndpointFailure {
    /// Raw IO boundary only. Typed failures and SSH diagnostics retain their cause.
    pub fn from_error(error: &io::Error) -> Self {
        if let Some(failure) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<Self>())
        {
            return failure.clone();
        }
        if let Some(diagnostic) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<SshFailureDiagnostic>())
        {
            return Self::from_ssh(diagnostic.clone());
        }
        if error.get_ref().is_some_and(|source| {
            source
                .downcast_ref::<shepr_platform::UnsafeSshRuntimeDirectory>()
                .is_some()
        }) {
            return Self::local_setup(error.to_string());
        }
        Self {
            cause: Cause::Io(error.kind()),
            message: error.to_string(),
        }
    }

    pub fn from_ssh(diagnostic: SshFailureDiagnostic) -> Self {
        Self {
            message: diagnostic.to_string(),
            cause: Cause::Ssh(diagnostic),
        }
    }

    pub fn incompatible(message: impl Into<String>) -> Self {
        Self {
            cause: Cause::Incompatible,
            message: message.into(),
        }
    }

    pub fn local_setup(message: impl Into<String>) -> Self {
        Self {
            cause: Cause::LocalSetup,
            message: message.into(),
        }
    }

    pub fn backpressure(message: impl Into<String>) -> Self {
        Self {
            cause: Cause::Backpressure,
            message: message.into(),
        }
    }

    pub fn retry(message: impl Into<String>) -> Self {
        Self {
            cause: Cause::Retry,
            message: message.into(),
        }
    }

    pub fn server_shutdown(reason: Option<shepr_protocol::ShutdownReason>) -> Self {
        let message = reason.as_ref().map_or_else(
            || "server shut down".to_owned(),
            |reason| format!("server shut down: {reason}"),
        );
        Self {
            cause: Cause::Shutdown(reason),
            message,
        }
    }

    pub fn shutdown_reason(&self) -> Option<&shepr_protocol::ShutdownReason> {
        match &self.cause {
            Cause::Shutdown(reason) => reason.as_ref(),
            _ => None,
        }
    }

    pub fn hint(&self, target: &SshTarget) -> Vec<String> {
        match &self.cause {
            Cause::Ssh(diagnostic) => crate::machine_ssh_error_hint(diagnostic, target),
            _ => Vec::new(),
        }
    }

    pub fn with_context(mut self, context: &str) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }

    pub fn disposition(&self) -> FailureDisposition {
        use FailureDisposition as D;
        match &self.cause {
            Cause::Ssh(diagnostic) => match diagnostic.failure {
                SshFailure::Authentication => D::Authentication,
                SshFailure::AuthenticationPending => D::PossibleAuthentication,
                SshFailure::HostKey => D::HostKey,
                SshFailure::Link => D::Offline,
                SshFailure::Compatibility => D::Incompatible,
                SshFailure::Other => D::Retry,
                SshFailure::LocalSetup
                | SshFailure::LocalConfiguration
                | SshFailure::RemoteRejected
                | SshFailure::Unrecognized => D::Repair,
            },
            Cause::Io(io::ErrorKind::InvalidData | io::ErrorKind::Unsupported)
            | Cause::Incompatible => D::Incompatible,
            Cause::Io(kind) if crate::is_ssh_link_error_kind(*kind) => D::Offline,
            Cause::LocalSetup => D::Repair,
            Cause::Io(_) | Cause::Backpressure | Cause::Retry | Cause::Shutdown(_) => D::Retry,
        }
    }

    pub fn disconnect_notice(&self) -> &'static str {
        if self.disposition().needs_attention() {
            return "connection failed; needs attention";
        }
        match self.cause {
            Cause::Backpressure => "local output queue filled; reconnecting",
            Cause::Shutdown(_) => "server shut down; reconnecting",
            Cause::Io(io::ErrorKind::TimedOut) => "connection timed out; reconnecting",
            Cause::Io(
                io::ErrorKind::UnexpectedEof
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::NotConnected,
            ) => "connection was lost; reconnecting",
            _ => "connection failed; reconnecting",
        }
    }

    /// Presentation adapter for the existing machine diagnostic UI. SSH hints
    /// retain their structured source; other failures never classify their text.
    pub fn diagnostic(&self) -> SshFailureDiagnostic {
        if let Cause::Ssh(diagnostic) = &self.cause {
            let mut diagnostic = diagnostic.clone();
            diagnostic.message.clone_from(&self.message);
            return diagnostic;
        }
        let failure = match self.disposition() {
            FailureDisposition::Incompatible => SshFailure::Compatibility,
            FailureDisposition::Repair => SshFailure::LocalSetup,
            FailureDisposition::Offline => SshFailure::Link,
            _ => SshFailure::Other,
        };
        SshFailureDiagnostic {
            failure,
            origin: match self.cause {
                Cause::Io(kind) => SshFailureOrigin::Io(kind),
                Cause::Incompatible => SshFailureOrigin::RemoteCompatibility,
                Cause::LocalSetup => SshFailureOrigin::LocalSetup,
                _ => SshFailureOrigin::Message,
            },
            message: self.message.clone(),
        }
    }
}

impl std::fmt::Display for EndpointFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EndpointFailure {}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(
                SshFailureDiagnostic::from_error(&error).disposition(),
                disposition
            );
        }
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
                crate::classify_check(Err(error)),
                crate::MachineCheck::Incompatible(_)
            ));
        }
    }

    #[test]
    fn ssh_authentication_keeps_its_prompt_and_hint_after_wrapping() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(crate::SSH_OWN_FAILURE_EXIT_CODE),
            "Permission denied (publickey).".into(),
        );
        let failure = EndpointFailure::from_ssh(diagnostic).with_context("handshake failed");
        let target = crate::SshTarget::parse("buildbox").expect("test precondition");
        let hints = failure.hint(&target);
        assert!(!hints.is_empty());
        let error = io::Error::other(failure);
        let restored = EndpointFailure::from_error(&error);
        assert_eq!(restored.disposition(), FailureDisposition::Authentication);
        assert_eq!(restored.hint(&target), hints);
        assert!(restored.diagnostic().requires_authentication());
    }

    #[test]
    fn shutdown_keeps_its_reason_and_queue_pressure_names_the_local_cause() {
        let failure = EndpointFailure::server_shutdown(Some(
            shepr_protocol::ShutdownReason::Message("updating".into()),
        ));
        let restored = EndpointFailure::from_error(&io::Error::other(failure));
        assert!(matches!(
            restored.shutdown_reason(),
            Some(shepr_protocol::ShutdownReason::Message(message)) if message == "updating"
        ));
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
