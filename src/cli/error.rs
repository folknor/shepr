use shepr_api::schema::ErrorResponse;

/// Every way a shepr invocation fails, rendered once by [`CliError::print`]
/// and turned into the process exit status by [`CliError::exit_code`]. Code
/// below `main` returns one of these rather than printing and exiting itself.
#[derive(Debug)]
pub(crate) enum CliError {
    Response(ErrorResponse),
    /// `server stop` could not stop the server.
    ServerStop(shepr_api::server_stop::ServerStopError),
    Usage(String),
    Io(std::io::Error),
    /// The configuration or the paths it resolves could not be loaded; one
    /// entry per diagnostic.
    Config(Vec<String>),
    /// A launch inside a shepr pane while nesting is disabled. `quip` is the
    /// closing line.
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
    pub(crate) fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            // A caller that ran a conditional stop over SSH tells "the server
            // was replaced, nothing stopped" from every other failure by this.
            Self::ServerStop(error) if error.is_boot_mismatch() => {
                shepr_api::server_stop::BOOT_MISMATCH_EXIT_CODE
            }
            _ => 1,
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
            Self::Config(diagnostics) => {
                eprintln!("shepr: configuration error:");
                for diagnostic in diagnostics {
                    eprintln!("  {diagnostic}");
                }
            }
            Self::Nested { quip } => {
                eprintln!("\x1b[1merror:\x1b[0m nested shepr is disabled by default.");
                eprintln!("see configuration if you want to enable it.");
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
/// restored the host terminal before returning, so its lines (forwarded
/// notices, then the message that ended the session) land on the restored
/// screen, each as `shepr: {line}`.
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

fn print_client_lines(exit: &shepr_client::ClientExit) {
    for line in exit.lines() {
        eprintln!("shepr: {line}");
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
            Self::Config(diagnostics) => {
                write!(f, "configuration error:\n  {}", diagnostics.join("\n  "))
            }
            Self::Nested { .. } => f.write_str("nested shepr is disabled"),
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
            Self::Client(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_usage_errors_exit_two() {
        assert_eq!(CliError::Usage("bad".into()).exit_code(), 2);
        for error in [
            CliError::Io(std::io::Error::other("io")),
            CliError::Config(vec!["bad key".into()]),
            CliError::Nested { quip: "deeper" },
            CliError::BridgeIdle,
        ] {
            assert_eq!(error.exit_code(), 1, "{error}");
        }
    }

    #[test]
    fn a_refused_conditional_stop_has_its_own_exit_code() {
        let refused = CliError::ServerStop(shepr_api::server_stop::ServerStopError::BootMismatch {
            label: "the server".into(),
            expected_boot_id: "1-1".into(),
            detail: "this server is boot 2-2".into(),
        });
        assert_eq!(
            refused.exit_code(),
            shepr_api::server_stop::BOOT_MISMATCH_EXIT_CODE
        );
        let failed = CliError::ServerStop(shepr_api::server_stop::ServerStopError::Protocol(
            "bad".into(),
        ));
        assert_eq!(failed.exit_code(), 1);
    }
}
