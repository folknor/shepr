use std::io;

/// What a finished client run leaves for the binary to print.
///
/// By the time a run returns, the client has restored the host terminal,
/// released its SSH resources and flushed its log, so these lines land on the
/// restored screen. The binary writes each one to stderr as `shepr: {line}`;
/// a line may itself span several terminal lines.
#[derive(Debug, Default)]
#[must_use = "the lines carry the message that ended the session, which the operator must see"]
pub struct ClientExit {
    ending: Option<SessionEnding>,
    message: Option<String>,
}

impl ClientExit {
    pub(crate) fn from_loop(ending: LoopExit) -> Self {
        Self::with_ending(SessionEnding::Loop(ending))
    }

    pub(crate) fn panicked(diagnostic: String) -> Self {
        Self::with_ending(SessionEnding::Panic(diagnostic))
    }

    fn with_ending(ending: SessionEnding) -> Self {
        Self {
            message: Some(ending.to_string()),
            ending: Some(ending),
        }
    }

    /// The message that ended the session, if any.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.message.as_deref().into_iter()
    }
}

#[derive(Debug)]
enum SessionEnding {
    Loop(LoopExit),
    Panic(String),
}

impl std::fmt::Display for SessionEnding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loop(exit) => write!(f, "{exit}"),
            Self::Panic(diagnostic) => f.write_str(diagnostic),
        }
    }
}

impl std::error::Error for SessionEnding {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Loop(exit) => Some(exit),
            Self::Panic(_) => None,
        }
    }
}

/// Why a client run failed; the binary prints it and exits nonzero.
#[derive(Debug)]
pub enum ClientRunError {
    /// The client failed while launching. If the host terminal had already
    /// been taken, its guard restored it before this was returned.
    Launch(io::Error),
    /// The session ended in failure after the host terminal was restored.
    /// The exit's last line is the failure.
    Session(ClientExit),
}

impl From<io::Error> for ClientRunError {
    fn from(error: io::Error) -> Self {
        Self::Launch(error)
    }
}

impl std::fmt::Display for ClientRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Launch(error) => write!(f, "{error}"),
            Self::Session(exit) => {
                for (index, line) in exit.lines().enumerate() {
                    if index > 0 {
                        f.write_str("\n")?;
                    }
                    f.write_str(line)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ClientRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Launch(error) => Some(error),
            Self::Session(exit) => exit
                .ending
                .as_ref()
                .map(|ending| -> &(dyn std::error::Error + 'static) { ending }),
        }
    }
}

/// The launch failure for an endpoint transport that could not be set up: a configured
/// machine's connector, or the first Local connection's reader and writer.
pub(crate) fn endpoint_setup_launch_error(error: &io::Error) -> ClientRunError {
    ClientRunError::Launch(io::Error::new(
        error.kind(),
        LaunchContext::EndpointSetup(shepr_remote::EndpointFailure::from_error(error)),
    ))
}

pub(crate) fn endpoint_connection_launch_error(error: io::Error) -> ClientRunError {
    ClientRunError::Launch(io::Error::new(
        error.kind(),
        LaunchContext::Connection(error),
    ))
}

// Launch remains an IO boundary because the binary also constructs it before
// calling the client. Contextual endpoint failures retain their typed cause here.
#[derive(Debug)]
enum LaunchContext {
    Connection(io::Error),
    EndpointSetup(shepr_remote::EndpointFailure),
}

impl std::fmt::Display for LaunchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(error) => write!(f, "endpoint connection setup failed: {error}"),
            Self::EndpointSetup(error) => {
                write!(f, "failed to set up configured endpoint transport: {error}")
            }
        }
    }
}

impl std::error::Error for LaunchContext {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connection(error) => Some(error),
            Self::EndpointSetup(error) => Some(error),
        }
    }
}

/// Failures while exchanging the preamble, hello and welcome.
#[derive(Debug)]
pub(crate) enum HandshakeError {
    /// Preparing the stream or encoding the local hello failed.
    EndpointSetup(io::Error),
    /// A connection could not be prepared or established.
    ConnectionFailed(io::Error),
    /// Server rejected our handshake.
    HandshakeRejected {
        error: shepr_protocol::HandshakeRefusal,
    },
    /// The peer did not identify itself as this build.
    Preamble(shepr_protocol::preamble::PreambleError),
    /// The first framed reply had the wrong message kind.
    UnexpectedWelcome,
    /// Server shut down.
    ServerShutdown {
        reason: shepr_protocol::ShutdownReason,
    },
    /// Lost connection to the server.
    ConnectionLost(io::Error),
    /// Protocol error (framing, deserialization).
    Protocol(shepr_protocol::FramingError),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::EndpointSetup(err) => {
                write!(f, "failed to set up configured endpoint transport: {err}")
            }
            HandshakeError::ConnectionFailed(err) => {
                write!(f, "endpoint connection setup failed: {err}")
            }
            HandshakeError::HandshakeRejected { error } => {
                write!(f, "server rejected handshake: {error}")
            }
            HandshakeError::Preamble(
                error @ shepr_protocol::preamble::PreambleError::DifferentBuild(_),
            ) => {
                // The preamble error already names the mismatch and both builds.
                write!(f, "{error}")
            }
            HandshakeError::Preamble(error) => write!(f, "protocol error: {error}"),
            HandshakeError::UnexpectedWelcome => {
                write!(f, "protocol error: expected endpoint welcome")
            }
            HandshakeError::ServerShutdown { reason } => {
                write!(f, "{reason}")
            }
            HandshakeError::ConnectionLost(err) => write!(f, "lost connection to server: {err}"),
            HandshakeError::Protocol(err) => write!(f, "protocol error: {err}"),
        }
    }
}

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for HandshakeError {}

impl From<shepr_protocol::FramingError> for HandshakeError {
    fn from(err: shepr_protocol::FramingError) -> Self {
        match err {
            shepr_protocol::FramingError::UnexpectedEof => HandshakeError::ConnectionLost(
                io::Error::new(io::ErrorKind::UnexpectedEof, "server closed connection"),
            ),
            shepr_protocol::FramingError::Io(err) => HandshakeError::ConnectionLost(err),
            err => HandshakeError::Protocol(err),
        }
    }
}

/// An established client loop's reason for ending.
#[derive(Debug)]
pub(crate) enum LoopExit {
    HostTerminal(io::Error),
    ServerShutdown {
        reason: shepr_protocol::ShutdownReason,
    },
    ConnectionLost(io::Error),
    Panicked,
}

impl std::fmt::Display for LoopExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostTerminal(error) => write!(f, "host terminal error: {error}"),
            Self::ServerShutdown { reason } => write!(f, "{reason}"),
            Self::ConnectionLost(error) => write!(f, "lost connection to server: {error}"),
            Self::Panicked => write!(f, "internal error: the client panicked"),
        }
    }
}

impl std::error::Error for LoopExit {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::HostTerminal(error) | Self::ConnectionLost(error) => Some(error),
            Self::ServerShutdown { .. } | Self::Panicked => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_lines_carry_the_ending_message() {
        let exit = ClientExit::from_loop(LoopExit::ServerShutdown {
            reason: shepr_protocol::ShutdownReason::Stopping,
        });
        let expected = shepr_protocol::ShutdownReason::Stopping.to_string();
        assert_eq!(exit.lines().collect::<Vec<_>>(), [expected.as_str()]);
        let failure = ClientRunError::Session(exit);
        assert_eq!(failure.to_string(), expected);
    }

    #[test]
    fn session_failure_retains_the_io_cause() {
        use std::error::Error as _;
        let exit = ClientExit::from_loop(LoopExit::ConnectionLost(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "peer reset",
        )));
        let error = ClientRunError::Session(exit);
        let ending = error.source().expect("typed session ending");
        let loop_exit = ending.source().expect("typed loop exit");
        let cause = loop_exit.source().expect("original IO failure");
        assert_eq!(cause.to_string(), "peer reset");
        assert_eq!(error.to_string(), "lost connection to server: peer reset");
    }

    #[test]
    fn a_clean_exit_prints_nothing() {
        let exit = ClientExit::default();
        assert_eq!(exit.lines().count(), 0);
    }
}
