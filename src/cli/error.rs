use shepr_api::schema::ErrorResponse;

/// Every way a shepr invocation fails, rendered once by [`CliError::print`]
/// and turned into the process exit status by [`CliError::exit_code`]. Code
/// below `main` returns one of these rather than printing and exiting itself.
#[derive(Debug)]
pub(crate) enum CliError {
    Response(ErrorResponse),
    Session(SessionCliError),
    Usage(String),
    Io(std::io::Error),
    /// A failure reported as prose, followed by operator hint lines.
    Failed {
        message: String,
        hints: Vec<String>,
    },
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

#[derive(Debug)]
pub(crate) enum SessionCliError {
    InvalidName(shepr_api::session::SessionError),
    Stop(shepr_api::session::SessionError),
    Delete(shepr_api::session::SessionError),
}

impl SessionCliError {
    fn code(&self) -> shepr_api::error::ApiErrorCode {
        match self {
            Self::InvalidName(_) => shepr_api::error::ApiErrorCode::InvalidSessionName,
            Self::Stop(_) => shepr_api::error::ApiErrorCode::SessionStopFailed,
            Self::Delete(_) => shepr_api::error::ApiErrorCode::SessionDeleteFailed,
        }
    }
}

impl std::fmt::Display for SessionCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(error) | Self::Stop(error) | Self::Delete(error) => error.fmt(f),
        }
    }
}

impl CliError {
    pub(crate) fn exit_code(&self) -> i32 {
        if matches!(self, Self::Usage(_)) { 2 } else { 1 }
    }

    pub(crate) fn print(&self) {
        match self {
            Self::Response(response) => match serde_json::to_string(response) {
                Ok(json) => eprintln!("{json}"),
                Err(error) => eprintln!("error: {error}"),
            },
            Self::Session(error) => eprintln!(
                "{}",
                serde_json::json!({
                    "error": shepr_api::schema::ErrorBody::new(
                        &error.code(),
                        error.to_string(),
                    )
                })
            ),
            Self::Usage(message) => {
                eprintln!("error: {message}");
                eprintln!("run 'shepr --help' for usage");
            }
            Self::Io(error) => eprintln!("error: {error}"),
            Self::Failed { message, hints } => {
                eprintln!("error: {message}");
                for hint in hints {
                    eprintln!("{hint}");
                }
            }
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

/// How a headless server that refused to start or stopped with an error is
/// reported. A server already holding the session, by either socket or by the
/// session data lock, reads the same to the operator.
impl From<shepr_server::server::headless::RunServerError> for CliError {
    fn from(error: shepr_server::server::headless::RunServerError) -> Self {
        use shepr_server::server::headless::RunServerError;
        const ALREADY_RUNNING: &str = "shepr server is already running";
        match error {
            RunServerError::AlreadyRunning { socket, path } => Self::Failed {
                message: ALREADY_RUNNING.into(),
                hints: vec![format!("{socket}: {}", path.display())],
            },
            RunServerError::Io(error) if error.kind() == std::io::ErrorKind::ResourceBusy => {
                Self::Failed {
                    message: ALREADY_RUNNING.into(),
                    hints: vec![error.to_string()],
                }
            }
            RunServerError::ManifestOverride(error) => Self::Config(vec![error.to_string()]),
            RunServerError::Io(error) => Self::Io(error),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Response(response) => f.write_str(&response.error.message),
            Self::Session(error) => error.fmt(f),
            Self::Usage(message) | Self::Failed { message, .. } => f.write_str(message),
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
            Self::Session(
                SessionCliError::InvalidName(error)
                | SessionCliError::Stop(error)
                | SessionCliError::Delete(error),
            ) => Some(error),
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
            CliError::Failed {
                message: "failed".into(),
                hints: vec!["hint: retry".into()],
            },
            CliError::Config(vec!["bad key".into()]),
            CliError::Nested { quip: "deeper" },
            CliError::BridgeIdle,
        ] {
            assert_eq!(error.exit_code(), 1, "{error}");
        }
    }

    #[test]
    fn invalid_session_name_preserves_its_error_source() {
        let error = shepr_api::session::SessionError::InvalidName("bad name".into());
        let cli_error = CliError::Session(SessionCliError::InvalidName(error));

        assert!(std::error::Error::source(&cli_error).is_some());
    }
}
