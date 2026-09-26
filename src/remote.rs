mod args;
#[cfg(test)]
mod attach;
mod bridge;
mod discovery;
mod host;
mod launch;
mod process;
mod saved;
mod server_lifecycle;
mod ssh;
mod ssh_agent;

use crate::machine::RemoteExecutable;
use bridge::*;
use discovery::*;
use launch::*;
use server_lifecycle::*;
use ssh::*;

pub(crate) use crate::machine::SshTarget;
pub(crate) use args::*;
#[cfg(test)]
pub(crate) use bridge::bridge_upload_cancellation_for_test;
pub(crate) use host::run_remote_client_bridge;
pub(crate) use launch::{
    check_saved_ssh, interactive_shell_command, prepare_saved_ssh, run_remote, shell_quote,
};
pub(crate) use saved::*;
pub(crate) use ssh::{release_ssh_resources_before_exit, ssh_authentication_command};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SshFailure {
    Authentication,
    HostKey,
    Link,
    StaleMetadata,
    Compatibility,
    Other,
}

/// A connection failure with its diagnostic class kept alongside its text.
/// Remote command output, IO errors and endpoint events all use this classifier.
#[derive(Clone, Debug)]
pub(crate) struct SshFailureDiagnostic {
    failure: SshFailure,
    origin: SshFailureOrigin,
    message: String,
}

#[derive(Clone, Copy, Debug)]
enum SshFailureOrigin {
    Io(std::io::ErrorKind),
    SshOutput(Option<i32>),
    Message,
}

impl SshFailure {
    pub(crate) fn requires_authentication(self) -> bool {
        self == Self::Authentication
    }

    pub(crate) fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::Authentication | Self::HostKey | Self::Compatibility
        )
    }
}

impl SshFailureDiagnostic {
    pub(crate) fn from_error(error: &std::io::Error) -> Self {
        if let Some(failure) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<Self>())
        {
            return failure.clone();
        }
        let message = error.to_string();
        let mut failure = classify_ssh_diagnostic(&message);
        if failure == SshFailure::Other {
            failure = if is_ssh_link_error_kind(error.kind()) {
                SshFailure::Link
            } else if is_attention_error_kind(error.kind()) {
                SshFailure::Compatibility
            } else {
                SshFailure::Other
            };
        }
        Self {
            failure,
            origin: SshFailureOrigin::Io(error.kind()),
            message,
        }
    }

    pub(crate) fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        let failure = classify_ssh_diagnostic(&message);
        Self {
            failure,
            origin: SshFailureOrigin::Message,
            message,
        }
    }

    pub(crate) fn from_ssh_output(exit_code: Option<i32>, message: String) -> Self {
        let mut failure = classify_ssh_diagnostic(&message);
        if failure == SshFailure::Other && exit_code == Some(SSH_OWN_FAILURE_EXIT_CODE) {
            failure = SshFailure::Link;
        }
        Self {
            failure,
            origin: SshFailureOrigin::SshOutput(exit_code),
            message,
        }
    }

    pub(crate) fn requires_authentication(&self) -> bool {
        self.failure.requires_authentication()
    }

    pub(crate) fn is_host_key(&self) -> bool {
        self.failure == SshFailure::HostKey
    }

    pub(crate) fn is_stale_metadata(&self) -> bool {
        self.failure == SshFailure::StaleMetadata
    }

    pub(crate) fn is_link_failure(&self) -> bool {
        if self.failure == SshFailure::Link {
            return true;
        }
        match self.origin {
            SshFailureOrigin::Io(kind) => is_ssh_link_error_kind(kind),
            SshFailureOrigin::SshOutput(exit_code) => exit_code == Some(SSH_OWN_FAILURE_EXIT_CODE),
            SshFailureOrigin::Message => false,
        }
    }

    pub(crate) fn needs_attention(&self) -> bool {
        self.failure.needs_attention()
    }

    fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for SshFailureDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for SshFailureDiagnostic {}

impl std::ops::Deref for SshFailureDiagnostic {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.message()
    }
}

fn classify_ssh_diagnostic(message: &str) -> SshFailure {
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
        || (message.contains("signing failed")
            && (message.contains("sign_and_send_pubkey") || message.contains("agent")))
    {
        return SshFailure::Authentication;
    }
    if message.contains(STALE_API_METADATA) {
        return SshFailure::StaleMetadata;
    }
    if [
        "permission denied",
        "unsupported remote platform",
        "not ready",
        "install or update",
        // A generic handshake can end during a transient restart; only a rejection needs attention.
        "handshake rejected",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        return SshFailure::Compatibility;
    }
    SshFailure::Other
}

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

fn is_attention_error_kind(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::InvalidData
            | std::io::ErrorKind::NotFound
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::Unsupported
    )
}

pub(crate) fn run_remote_api_bridge(
    check: bool,
    paths: &crate::config::AppPaths,
) -> std::io::Result<()> {
    if check {
        return Ok(());
    }
    let path = crate::api::socket_path(paths);
    let stream = crate::ipc::connect_local_stream(&path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to connect to remote Shepr API socket {}: {error}",
                path.display()
            ),
        )
    })?;
    crate::platform::forward_remote_bridge_stdio(stream, false)
}

pub(crate) fn print_saved_ssh_error_hint(err: &std::io::Error, target: &str) {
    if is_remote_host_key_error(err) {
        eprintln!(
            "hint: saved machines use strict host-key checking; add the host key to the configured known_hosts file, then retry."
        );
    } else {
        print_remote_error_hint(err, target);
    }
}

pub(crate) fn print_remote_error_hint(err: &std::io::Error, target: &str) {
    if is_remote_auth_error(err) {
        eprintln!(
            "hint: verify SSH access first with `{}`.",
            ssh_check_command(target)
        );
        eprintln!(
            "hint: if your SSH key has a passphrase, load it into ssh-agent with `ssh-add` before retrying."
        );
    }
}

fn is_remote_host_key_error(err: &std::io::Error) -> bool {
    SshFailureDiagnostic::from_error(err).is_host_key()
}

fn is_remote_auth_error(err: &std::io::Error) -> bool {
    SshFailureDiagnostic::from_error(err).requires_authentication()
}

pub(crate) fn ssh_error_requires_authentication(message: &str) -> bool {
    SshFailureDiagnostic::from_message(message).requires_authentication()
}

fn ssh_check_command(target: &str) -> String {
    format!("ssh {}", shell_quote(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_host_key_error_matches_ssh_diagnostics() {
        for message in [
            "Host key verification failed.",
            "REMOTE HOST IDENTIFICATION HAS CHANGED!",
        ] {
            assert!(is_remote_host_key_error(&std::io::Error::other(message)));
        }
        assert!(!is_remote_host_key_error(&std::io::Error::other(
            "server closed connection"
        )));
    }

    #[test]
    fn remote_auth_error_matches_ssh_auth_denied() {
        let err = std::io::Error::other(
            "remote platform detection failed: user@host: Permission denied (publickey).",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_matches_keyboard_interactive_denied() {
        let err = std::io::Error::other(
            "remote server status failed: user@host: Permission denied (keyboard-interactive).",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_ignores_non_auth_errors() {
        let err = std::io::Error::other("remote platform detection failed: unsupported platform");

        assert!(!is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_matches_case_insensitive_signing_failures() {
        let err = std::io::Error::other(
            "SIGN_AND_SEND_PUBKEY: SIGNING FAILED for ED25519 from agent: agent refused operation",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_does_not_treat_host_key_errors_as_authentication() {
        let err =
            std::io::Error::other("Permission denied (publickey). Host key verification failed.");

        assert!(!is_remote_auth_error(&err));
    }

    #[test]
    fn ssh_check_command_quotes_remote_target() {
        assert_eq!(ssh_check_command("host name"), "ssh 'host name'");
    }
}
