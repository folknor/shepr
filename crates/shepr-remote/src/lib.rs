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
#[path = "remote/ssh.rs"]
mod ssh;

use crate::machine::RemoteExecutable;
use bridge::*;
use discovery::*;
use launch::*;
use server_lifecycle::*;
use ssh::*;

pub use crate::machine::SshTarget;
pub use args::*;
pub use bridge::{BridgeUpload, BridgeUploadEnd};
pub use host::run_remote_client_bridge;
pub use launch::{RemoteStop, interactive_shell_command, shell_quote, stop_remote_server};
pub use machine_ssh::*;
pub use preflight::{
    MachineCheck, MachineSshPreflight, PreflightOutcome, PreflightSsh, RestartDecision,
    RestartResult, classify_check, preflight, restart_different_builds,
};
pub use server_lifecycle::{DifferentBuildServer, MachineSshCheck};
pub use ssh::{release_ssh_resources_before_exit, ssh_authentication_command};

/// What a failed connection attempt established. The class also says who can
/// fix it: [`Self::needs_attention`] is false only for failures a retry can
/// clear by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SshFailure {
    /// The remote refused every offered credential.
    Authentication,
    /// A bounded SSH command ended before returning, so foreground SSH may
    /// need to wait for interactive authentication or security-key presence.
    AuthenticationPending,
    /// The remote's host key is unknown or changed.
    HostKey,
    /// Local SSH setup or process creation failed before a remote result.
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

impl SshFailure {
    fn needs_attention(self) -> bool {
        // A round-trip timeout is only a hint that authentication may be
        // waiting. Startup preflight acts on that hint before this question is
        // asked; a running client cannot prompt, so it keeps retrying as it
        // does for a link failure instead of parking the machine in Attention.
        !matches!(self, Self::Link | Self::AuthenticationPending | Self::Other)
    }
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
    CommandTimeout,
    LocalSetup,
    RemoteCompatibility,
    Message,
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
        if error.get_ref().is_some_and(|source| {
            source
                .downcast_ref::<shepr_platform::UnsafeSshRuntimeDirectory>()
                .is_some()
        }) {
            return Self {
                failure: SshFailure::LocalSetup,
                origin: SshFailureOrigin::LocalSetup,
                message,
            };
        }
        let failure = if is_ssh_link_error_kind(error.kind()) {
            SshFailure::Link
        } else if matches!(
            error.kind(),
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::Unsupported
        ) {
            // These kinds also carry remote protocol and install refusals from
            // callers outside this crate. Local sources are wrapped explicitly.
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
        let failure = if exit_code == Some(SSH_OWN_FAILURE_EXIT_CODE) {
            classify_ssh_diagnostic(&message)
        } else {
            SshFailure::Other
        };
        Self {
            failure,
            origin: SshFailureOrigin::SshOutput(exit_code),
            message,
        }
    }

    /// Adds display context while retaining this diagnostic's structured class.
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.message = format!("{}: {}", context.into(), self.message);
        self
    }

    pub fn requires_authentication(&self) -> bool {
        self.failure == SshFailure::Authentication
    }

    fn may_require_interactive_authentication(&self) -> bool {
        matches!(
            self.failure,
            SshFailure::Authentication | SshFailure::AuthenticationPending
        )
    }

    pub(crate) fn is_authentication_wait_timeout(&self) -> bool {
        self.failure == SshFailure::AuthenticationPending
    }

    pub(crate) fn is_remote_compatibility(&self) -> bool {
        matches!(self.origin, SshFailureOrigin::RemoteCompatibility)
    }

    pub(crate) fn is_local_setup_failure(&self) -> bool {
        self.failure == SshFailure::LocalSetup
    }

    pub(crate) fn authentication_wait_timeout() -> Self {
        Self {
            failure: SshFailure::AuthenticationPending,
            origin: SshFailureOrigin::CommandTimeout,
            message: "SSH command timed out before returning a remote result; interactive authentication may be needed"
                .into(),
        }
    }

    pub fn is_host_key(&self) -> bool {
        self.failure == SshFailure::HostKey
    }

    /// Whether the attempt failed before any remote command produced a result:
    /// SSH itself failed (whatever the cause, authentication and host key
    /// included), a bounded SSH command timed out, or a typed IO error says the
    /// link was never made or was lost. Discovery and the bridge use it so no
    /// such failure is read as a remote command's answer. It says nothing about
    /// whether a retry helps; that is [`Self::is_transient_network_failure`].
    pub fn failed_before_remote_result(&self) -> bool {
        match self.origin {
            SshFailureOrigin::Io(kind) => is_ssh_link_error_kind(kind),
            SshFailureOrigin::SshOutput(exit_code) => exit_code == Some(SSH_OWN_FAILURE_EXIT_CODE),
            SshFailureOrigin::CommandTimeout => true,
            SshFailureOrigin::LocalSetup
            | SshFailureOrigin::RemoteCompatibility
            | SshFailureOrigin::Message => false,
        }
    }

    /// Whether OpenSSH exited with its own failure status before returning a
    /// remote command result.
    pub fn is_ssh_process_failure(&self) -> bool {
        matches!(
            self.origin,
            SshFailureOrigin::SshOutput(Some(SSH_OWN_FAILURE_EXIT_CODE))
        )
    }

    /// The remote command's own nonzero exit status, when ssh ran the command
    /// and it failed (ssh's own exit 255 is not one).
    pub fn remote_exit_code(&self) -> Option<i32> {
        match self.origin {
            SshFailureOrigin::SshOutput(Some(code)) if code != SSH_OWN_FAILURE_EXIT_CODE => {
                Some(code)
            }
            SshFailureOrigin::Io(_)
            | SshFailureOrigin::SshOutput(_)
            | SshFailureOrigin::CommandTimeout
            | SshFailureOrigin::LocalSetup
            | SshFailureOrigin::RemoteCompatibility
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

pub(crate) fn local_setup_error(context: &str, error: std::io::Error) -> std::io::Error {
    if error.get_ref().is_some_and(|source| {
        source
            .downcast_ref::<shepr_platform::UnsafeSshRuntimeDirectory>()
            .is_some()
    }) {
        return error;
    }
    let message = format!("{context}: {error}");
    std::io::Error::new(
        error.kind(),
        SshFailureDiagnostic {
            failure: SshFailure::LocalSetup,
            origin: SshFailureOrigin::LocalSetup,
            message,
        },
    )
}

pub(crate) fn remote_compatibility_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        SshFailureDiagnostic {
            failure: SshFailure::Compatibility,
            origin: SshFailureOrigin::RemoteCompatibility,
            message: message.into(),
        },
    )
}

/// Operator hint lines for a failed configured-machine SSH operation, one per
/// line and without a trailing newline. Empty when there is no hint. The
/// binary renders them; this crate does not print.
pub fn machine_ssh_error_hint(err: &SshFailureDiagnostic, target: &str) -> Vec<String> {
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

fn remote_error_hint_for_failure(failure: &SshFailureDiagnostic, target: &str) -> Vec<String> {
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
            assert!(!machine_ssh_error_hint(&failure, "host").is_empty());
        }
        assert!(
            machine_ssh_error_hint(
                &SshFailureDiagnostic::from_message("server closed connection"),
                "host"
            )
            .is_empty()
        );
    }

    #[test]
    fn remote_auth_error_matches_ssh_auth_denied() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "remote platform detection failed: user@host: Permission denied (publickey).".into(),
        );
        assert!(!remote_error_hint_for_failure(&diagnostic, "host").is_empty());
    }

    #[test]
    fn remote_auth_error_matches_keyboard_interactive_denied() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "remote server status failed: user@host: Permission denied (keyboard-interactive)."
                .into(),
        );
        assert!(!remote_error_hint_for_failure(&diagnostic, "host").is_empty());
    }

    #[test]
    fn remote_auth_error_ignores_non_auth_errors() {
        let diagnostic = SshFailureDiagnostic::from_message(
            "remote platform detection failed: unsupported platform",
        );

        assert!(remote_error_hint_for_failure(&diagnostic, "host").is_empty());
    }

    #[test]
    fn remote_auth_error_matches_case_insensitive_signing_failures() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "SIGN_AND_SEND_PUBKEY: SIGNING FAILED for ED25519 from agent: agent refused operation"
                .into(),
        );
        assert!(!remote_error_hint_for_failure(&diagnostic, "host").is_empty());
    }

    #[test]
    fn remote_auth_error_does_not_treat_host_key_errors_as_authentication() {
        let diagnostic = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "Permission denied (publickey). Host key verification failed.".into(),
        );
        assert!(remote_error_hint_for_failure(&diagnostic, "host").is_empty());
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
            let diagnostic = SshFailureDiagnostic::from_ssh_output(
                Some(SSH_OWN_FAILURE_EXIT_CODE),
                message.into(),
            );
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
        assert_eq!(ssh_check_command("host name"), "ssh 'host name'");
    }
}
