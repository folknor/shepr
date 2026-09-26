mod args;
mod attach;
mod host;
mod process;
mod restart_policy;
mod saved;
mod ssh_agent;

pub(crate) use args::*;
pub(crate) use attach::*;
pub(crate) use host::run_remote_client_bridge;
pub(crate) use saved::*;

pub(crate) fn run_remote_api_bridge(args: &[String]) -> std::io::Result<()> {
    match args {
        [] => {
            let path = crate::api::socket_path();
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
        [flag] if flag == "--check" => {
            println!("shepr-api-bridge-v1");
            Ok(())
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: shepr remote-api-bridge [--check]",
        )),
    }
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
    let message = err.to_string().to_ascii_lowercase();
    message.contains("host key verification failed")
        || message.contains("remote host identification has changed")
}

fn is_remote_auth_error(err: &std::io::Error) -> bool {
    ssh_error_requires_authentication(&err.to_string())
}

fn ssh_check_command(target: &str) -> String {
    format!("ssh {}", shell_quote(target))
}

use crate::platform::shell_quote;

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
