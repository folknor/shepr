#[path = "remote/args.rs"]
mod args;
#[cfg(test)]
#[path = "remote/attach.rs"]
mod attach;
#[path = "remote/autodetect.rs"]
pub mod autodetect;
#[path = "remote/bridge.rs"]
mod bridge;
#[path = "remote/discovery.rs"]
mod discovery;
#[path = "remote/host.rs"]
mod host;
#[path = "remote/launch.rs"]
mod launch;
pub mod machine;
#[path = "remote/process.rs"]
mod process;
#[path = "remote/saved.rs"]
mod saved;
#[path = "remote/server_lifecycle.rs"]
mod server_lifecycle;
#[path = "remote/ssh.rs"]
mod ssh;
#[path = "remote/ssh_agent.rs"]
mod ssh_agent;

use crate::machine::RemoteExecutable;
use bridge::*;
use discovery::*;
use launch::*;
use server_lifecycle::*;
use ssh::*;

pub use crate::machine::SshTarget;
pub use args::*;
#[cfg(any(test, feature = "test-support"))]
pub use bridge::bridge_upload_cancellation_for_test;
pub use host::run_remote_client_bridge;
pub use launch::{
    check_saved_ssh, interactive_shell_command, prepare_saved_ssh, run_remote, shell_quote,
};
pub use saved::*;
pub use ssh::{release_ssh_resources_before_exit, ssh_authentication_command};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshFailure {
    Authentication,
    HostKey,
    Link,
    StaleMetadata,
    Compatibility,
    Other,
}

/// A connection failure with its diagnostic class kept alongside its text.
/// Text classification is reserved for the SSH process boundary; errors and
/// endpoint events carry this value after that point.
#[derive(Clone, Debug)]
pub struct SshFailureDiagnostic {
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
    pub fn requires_authentication(self) -> bool {
        self == Self::Authentication
    }

    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::Authentication | Self::HostKey | Self::Compatibility
        )
    }
}

impl SshFailureDiagnostic {
    pub fn from_error(error: &std::io::Error) -> Self {
        if let Some(failure) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<Self>())
        {
            return failure.clone();
        }
        let message = error.to_string();
        let failure = if is_ssh_link_error_kind(error.kind()) {
            SshFailure::Link
        } else if is_attention_error_kind(error.kind()) {
            SshFailure::Compatibility
        } else {
            SshFailure::Other
        };
        Self {
            failure,
            origin: SshFailureOrigin::Io(error.kind()),
            message,
        }
    }

    pub fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            failure: SshFailure::Other,
            origin: SshFailureOrigin::Message,
            message,
        }
    }

    pub fn from_ssh_output(exit_code: Option<i32>, message: String) -> Self {
        let failure = if exit_code == Some(STALE_API_METADATA_EXIT_CODE)
            && message.contains(STALE_API_METADATA)
        {
            SshFailure::StaleMetadata
        } else if exit_code == Some(SSH_OWN_FAILURE_EXIT_CODE) {
            match classify_ssh_diagnostic(&message) {
                SshFailure::Other => SshFailure::Link,
                failure => failure,
            }
        } else {
            SshFailure::Other
        };
        Self {
            failure,
            origin: SshFailureOrigin::SshOutput(exit_code),
            message,
        }
    }

    pub fn failure(&self) -> SshFailure {
        self.failure
    }

    /// Adds display context while retaining this diagnostic's structured class.
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.message = format!("{}: {}", context.into(), self.message);
        self
    }

    pub fn requires_authentication(&self) -> bool {
        self.failure.requires_authentication()
    }

    pub fn is_host_key(&self) -> bool {
        self.failure == SshFailure::HostKey
    }

    pub fn is_stale_metadata(&self) -> bool {
        self.failure == SshFailure::StaleMetadata
    }

    pub fn is_link_failure(&self) -> bool {
        if self.failure == SshFailure::Link {
            return true;
        }
        match self.origin {
            SshFailureOrigin::Io(kind) => is_ssh_link_error_kind(kind),
            SshFailureOrigin::SshOutput(exit_code) => exit_code == Some(SSH_OWN_FAILURE_EXIT_CODE),
            SshFailureOrigin::Message => false,
        }
    }

    pub fn needs_attention(&self) -> bool {
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
        || (message.contains("signing failed")
            && (message.contains("sign_and_send_pubkey") || message.contains("agent")))
    {
        return SshFailure::Authentication;
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

pub fn run_remote_api_bridge(check: bool, paths: &shepr_config::AppPaths) -> std::io::Result<()> {
    if check {
        return Ok(());
    }
    let path = shepr_api::socket_path(paths);
    let stream = shepr_platform::ipc::connect_local_stream(&path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to connect to remote Shepr API socket {}: {error}",
                path.display()
            ),
        )
    })?;
    shepr_platform::forward_remote_bridge_stdio(stream, false)
}

pub fn print_saved_ssh_error_hint(err: &std::io::Error, target: &str) {
    if is_remote_host_key_error(err) {
        eprintln!(
            "hint: saved machines use strict host-key checking; add the host key to the configured known_hosts file, then retry."
        );
    } else {
        print_remote_error_hint(err, target);
    }
}

pub fn print_remote_error_hint(err: &std::io::Error, target: &str) {
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
            let failure = SshFailureDiagnostic::from_ssh_output(
                Some(SSH_OWN_FAILURE_EXIT_CODE),
                message.into(),
            );
            assert!(is_remote_host_key_error(&std::io::Error::other(failure)));
        }
        assert!(!is_remote_host_key_error(&std::io::Error::other(
            "server closed connection"
        )));
    }

    #[test]
    fn remote_auth_error_matches_ssh_auth_denied() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "remote platform detection failed: user@host: Permission denied (publickey).".into(),
        );
        let err = std::io::Error::other(diagnostic);

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_matches_keyboard_interactive_denied() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "remote server status failed: user@host: Permission denied (keyboard-interactive)."
                .into(),
        );
        let err = std::io::Error::other(diagnostic);

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_ignores_non_auth_errors() {
        let err = std::io::Error::other("remote platform detection failed: unsupported platform");

        assert!(!is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_matches_case_insensitive_signing_failures() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "SIGN_AND_SEND_PUBKEY: SIGNING FAILED for ED25519 from agent: agent refused operation"
                .into(),
        );
        let err = std::io::Error::other(diagnostic);

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_does_not_treat_host_key_errors_as_authentication() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "Permission denied (publickey). Host key verification failed.".into(),
        );
        let err = std::io::Error::other(diagnostic);

        assert!(!is_remote_auth_error(&err));
    }

    #[test]
    fn ssh_check_command_quotes_remote_target() {
        assert_eq!(ssh_check_command("host name"), "ssh 'host name'");
    }
}
