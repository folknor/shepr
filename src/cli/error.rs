use shepr_api::schema::ErrorResponse;

use crate::ProcessExit;

/// Every way a shepr invocation fails, rendered once by [`CliError::print`]
/// and turned into the process exit status by [`CliError::exit_status`]. Code
/// below `main` returns one of these rather than printing and exiting itself.
#[derive(Debug)]
pub(crate) enum CliError {
    Response(ErrorResponse),
    /// `shepr stop` could not stop the server.
    ServerStop(shepr_launch::stop::ServerStopError),
    /// A local CLI failure that does not belong in the server API vocabulary.
    Message(String),
    Usage(String),
    Io(std::io::Error),
    Launch(shepr_launch::local_server::LaunchError),
    /// The host terminal has no usable geometry, so the TUI cannot attach. The
    /// only cause is the terminal size query's io error.
    Terminal(std::io::Error),
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
            // A pane without a terminal is an ordinary answer to a detect
            // request, so it reads as a plain sentence, not as an envelope.
            Self::Response(response)
                if response.error.code
                    == shepr_api::error::ApiErrorCode::PaneTerminalUnavailable =>
            {
                eprintln!("{}", response.error.message);
            }
            Self::Response(response) => eprintln!("error: {}", response.error.message),
            Self::ServerStop(error) => eprintln!("error: {error}"),
            Self::Message(message) => eprintln!("error: {message}"),
            Self::Usage(message) => {
                eprintln!("error: {message}");
                eprintln!("{}", shepr_launch::guidance::usage_hint());
            }
            Self::Io(error) => eprintln!("error: {error}"),
            Self::Launch(error) => eprintln!("shepr: {error}"),
            Self::Terminal(error) => eprintln!("shepr: {error}"),
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
                let (bold, dim, reset) =
                    if super::color_enabled(&std::io::stderr()).unwrap_or_default() {
                        ("\x1b[1m", "\x1b[2m", "\x1b[0m")
                    } else {
                        ("", "", "")
                    };
                eprintln!(
                    "{bold}error:{reset} {}",
                    shepr_launch::guidance::NESTED_REFUSAL
                );
                eprintln!();
                eprintln!("{dim}\"{quip}\"{reset}");
            }
            Self::Client(shepr_client::ClientRunError::Launch(error)) => {
                eprintln!("shepr: {error}");
            }
            Self::Client(shepr_client::ClientRunError::Session(exit)) => {
                print_client_lines(exit);
            }
        }
    }
}

/// Turns a finished client run into the command's result. The client has
/// restored the host terminal before returning, so its optional session
/// message lands on the restored screen as `shepr: {message}`, and after a
/// user detach, the guidance on getting back to the server at `address`.
/// `machines_configured` says whether configured machines' servers were left
/// running too.
pub(crate) fn finish_client(
    outcome: Result<shepr_client::ClientExit, shepr_client::ClientRunError>,
    address: &shepr_paths::ServerAddress,
    machines_configured: bool,
) -> Result<i32, CliError> {
    match outcome {
        Ok(exit) => {
            print_client_lines(&exit);
            if let Some(guidance) = detach_notice(&exit, address, machines_configured) {
                print_stderr_line(&guidance);
            }
            Ok(0)
        }
        Err(error) => Err(CliError::Client(error)),
    }
}

/// The guidance a finished client run prints after the user detached: how to
/// attach again and how to stop the server. A run that ended any other way (a
/// server shutdown, a signal, a lost terminal) prints none; a failed run never
/// reaches here.
fn detach_notice(
    exit: &shepr_client::ClientExit,
    address: &shepr_paths::ServerAddress,
    machines_configured: bool,
) -> Option<String> {
    exit.detached()
        .then(|| shepr_launch::guidance::detach_guidance(address, machines_configured))
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

/// One fallible stderr line, for the same reason as [`print_client_lines`].
fn print_stderr_line(line: &str) {
    use std::io::Write as _;
    writeln!(std::io::stderr().lock(), "{line}").ok();
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
            Self::Message(message) | Self::Usage(message) => f.write_str(message),
            Self::Io(error) | Self::Terminal(error) => error.fmt(f),
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
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ServerStop(error) => Some(error),
            Self::Io(error) | Self::Terminal(error) => Some(error),
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
            CliError::Message("local error".into()),
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

    /// A user detach prints how to get back and how to stop the server, in
    /// this build's spelling; a run that ended any other way (here a clean
    /// quit) prints nothing. Which runs count as a detach is the client's
    /// `ClientExit::detached`.
    #[test]
    fn only_a_detach_prints_detach_guidance() {
        let address = shepr_paths::ServerAddress::for_runtime_dir(
            std::path::Path::new("/run/user/1/shepr"),
            None,
        )
        .expect("valid test socket path");
        let entrypoint = shepr_launch::guidance::operator_entrypoint();
        let detached = shepr_client::ClientExit::user_detach();
        assert_eq!(
            detach_notice(&detached, &address, false).as_deref(),
            Some(
                format!(
                    "Detached. Run `{entrypoint}` to re-attach, or `{entrypoint} stop` to stop the local server and everything running in it."
                )
                .as_str()
            )
        );
        let with_machines =
            detach_notice(&detached, &address, true).expect("a detach prints guidance");
        assert!(
            with_machines.ends_with(" Servers on configured machines keep running."),
            "{with_machines}"
        );

        let quit = shepr_client::ClientExit::default();
        assert!(!quit.detached());
        assert_eq!(detach_notice(&quit, &address, false), None);
        assert_eq!(detach_notice(&quit, &address, true), None);
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
