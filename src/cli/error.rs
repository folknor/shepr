use shepr_api::schema::ErrorResponse;

use crate::ProcessExit;

/// Every way a shepr invocation fails, rendered once by [`CliError::print`]
/// and turned into the process exit status by [`CliError::exit_status`]. Code
/// below `main` returns one of these rather than printing and exiting itself.
#[derive(Debug)]
pub(crate) enum CliError {
    Response(ErrorResponse),
    /// `server stop` could not stop the server.
    ServerStop(shepr_launch::stop::ServerStopError),
    Usage(String),
    Io(std::io::Error),
    Launch(shepr_launch::local_server::LaunchError),
    /// `client.toml` could not be loaded; one entry per diagnostic, each
    /// carrying its file, key path and reason.
    Config(Vec<shepr_config::ConfigDiagnostic>),
    /// The application paths (XDG directories, socket target, pane markers)
    /// could not be resolved.
    Paths(shepr_paths::PathsError),
    /// A TUI or client launch inside a pane of a server of this build profile.
    /// `quip` is the closing line.
    Nested {
        quip: &'static str,
    },
    /// A client run that failed; see [`finish_client`].
    Client(shepr_client::ClientRunError),
    /// The remote bridge's idle watchdog fired. Nothing is printed: stdout
    /// belongs to the relay, and the far side already stopped listening.
    BridgeIdle,
}

impl CliError {
    pub(crate) fn exit_status(&self) -> ProcessExit {
        match self {
            Self::Usage(_) => ProcessExit::Usage,
            // A caller that ran a conditional stop over SSH can identify a
            // different boot at the stop request or during shutdown with this.
            Self::ServerStop(error) if error.is_boot_mismatch() => {
                ProcessExit::Stop(shepr_launch::stop::ServerStopExit::BootMismatch)
            }
            // Likewise "there was no server to stop" (it had already exited).
            Self::ServerStop(error) if error.is_not_running() => {
                ProcessExit::Stop(shepr_launch::stop::ServerStopExit::NoServer)
            }
            _ => ProcessExit::Failed,
        }
    }

    pub(crate) fn print(&self) {
        match self {
            Self::Response(response) => match serde_json::to_string(response) {
                Ok(json) => eprintln!("{json}"),
                Err(error) => eprintln!("error: {error}"),
            },
            Self::ServerStop(error) => eprintln!(
                "{}",
                serde_json::json!({
                    "error": shepr_api::schema::ErrorBody::new(
                        &error.error_code(),
                        error.to_string(),
                    )
                })
            ),
            Self::Usage(message) => {
                eprintln!("error: {message}");
                eprintln!("run 'shepr --help' for usage");
            }
            Self::Io(error) => eprintln!("error: {error}"),
            Self::Launch(error) => eprintln!("shepr: {error}"),
            Self::Config(diagnostics) => {
                eprintln!("shepr: configuration error:");
                for diagnostic in diagnostics {
                    eprintln!("  {diagnostic}");
                }
            }
            Self::Paths(error) => {
                eprintln!("shepr: application paths could not be resolved:");
                for diagnostic in error.messages() {
                    eprintln!("  {diagnostic}");
                }
            }
            Self::Nested { quip } => {
                eprintln!(
                    "\x1b[1merror:\x1b[0m shepr does not run inside a pane of a server of its own build profile."
                );
                eprintln!();
                eprintln!("\x1b[2m\"{quip}\"\x1b[0m");
            }
            Self::Client(shepr_client::ClientRunError::Launch(error)) => {
                eprintln!("shepr: {error}");
            }
            Self::Client(shepr_client::ClientRunError::Session(exit)) => {
                print_client_lines(exit);
            }
            Self::BridgeIdle => {}
        }
    }
}

/// Turns a finished client run into the command's result. The client has
/// restored the host terminal before returning, so its optional session
/// message lands on the restored screen as `shepr: {message}`.
pub(crate) fn finish_client(
    outcome: Result<shepr_client::ClientExit, shepr_client::ClientRunError>,
) -> Result<i32, CliError> {
    match outcome {
        Ok(exit) => {
            print_client_lines(&exit);
            Ok(0)
        }
        Err(error) => Err(CliError::Client(error)),
    }
}

/// Fallible writes: a client's last lines can be a panic diagnostic, printed
/// after the client has finalized, and a stderr failure here must not panic
/// again outside every catch.
fn print_client_lines(exit: &shepr_client::ClientExit) {
    use std::io::Write as _;
    let mut stderr = std::io::stderr().lock();
    for line in exit.lines() {
        if writeln!(stderr, "shepr: {line}").is_err() {
            return;
        }
    }
}

/// An operator notice on stderr: progress or status, never a failure, so it
/// does not change the exit status.
pub(crate) fn print_notice(notice: &dyn std::fmt::Display) {
    eprintln!("{notice}");
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Response(response) => f.write_str(&response.error.message),
            Self::ServerStop(error) => error.fmt(f),
            Self::Usage(message) => f.write_str(message),
            Self::Io(error) => error.fmt(f),
            Self::Launch(error) => error.fmt(f),
            Self::Config(diagnostics) => {
                f.write_str("configuration error:")?;
                for diagnostic in diagnostics {
                    write!(f, "\n  {diagnostic}")?;
                }
                Ok(())
            }
            Self::Paths(error) => {
                f.write_str("application paths could not be resolved:")?;
                for diagnostic in error.messages() {
                    write!(f, "\n  {diagnostic}")?;
                }
                Ok(())
            }
            Self::Nested { .. } => f.write_str("nested shepr is refused"),
            Self::Client(error) => error.fmt(f),
            Self::BridgeIdle => f.write_str("remote bridge idle timeout expired"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ServerStop(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Launch(error) => Some(error),
            Self::Client(error) => Some(error),
            Self::Paths(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<shepr_paths::PathsError> for CliError {
    fn from(error: shepr_paths::PathsError) -> Self {
        Self::Paths(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_usage_errors_exit_two() {
        assert_eq!(CliError::Usage("bad".into()).exit_status().code(), 2);
        for error in [
            CliError::Io(std::io::Error::other("io")),
            CliError::Config(vec![shepr_config::ConfigDiagnostic::parse("bad key")]),
            CliError::Nested { quip: "deeper" },
            CliError::BridgeIdle,
        ] {
            assert_eq!(error.exit_status().code(), 1, "{error}");
        }
    }

    #[test]
    fn a_refused_conditional_stop_has_its_own_exit_code() {
        let refused = CliError::ServerStop(shepr_launch::stop::ServerStopError::BootMismatch {
            label: "the server".into(),
            expected_boot_id: "1-1".parse().expect("boot identity"),
            detail: "this server is boot 2-2".into(),
        });
        assert_eq!(
            refused.exit_status().code(),
            ProcessExit::Stop(shepr_launch::stop::ServerStopExit::BootMismatch).code()
        );
        let replaced = CliError::ServerStop(shepr_launch::stop::ServerStopError::OccupantChanged {
            label: "the server".into(),
            expected_boot_id: "1-1".parse().expect("expected boot identity"),
            actual_boot_id: "2-2".parse().expect("replacement boot identity"),
        });
        assert_eq!(refused.exit_status().code(), 3);
        assert_eq!(replaced.exit_status().code(), 3);
        let failed =
            CliError::ServerStop(shepr_launch::stop::ServerStopError::Protocol("bad".into()));
        assert_eq!(failed.exit_status().code(), 1);
    }

    #[test]
    fn a_stop_with_no_server_has_its_own_exit_code() {
        let none = CliError::ServerStop(shepr_launch::stop::ServerStopError::NotRunning {
            label: "server".into(),
            path: "/run/shepr/shepr.sock".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        });
        assert_eq!(
            none.exit_status().code(),
            ProcessExit::Stop(shepr_launch::stop::ServerStopExit::NoServer).code()
        );
    }
}
