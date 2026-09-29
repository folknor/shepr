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
    message: Option<String>,
}

impl ClientExit {
    pub(crate) fn new(message: Option<String>) -> Self {
        Self { message }
    }

    /// The message that ended the session, if any.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.message.as_deref().into_iter()
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

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for ClientRunError {}

/// Errors that can occur during client operation.
#[derive(Debug)]
pub enum ClientError {
    /// A saved endpoint's local SSH transport cannot be configured for this launch.
    EndpointSetup(io::Error),
    /// Could not connect to the server's client socket.
    ConnectionFailed(io::Error),
    /// A host terminal write failed while updating terminal modes or output.
    HostTerminal(io::Error),
    /// Server rejected our handshake.
    HandshakeRejected {
        error: shepr_protocol::HandshakeRefusal,
    },
    /// The peer did not send a valid build preamble.
    Preamble(shepr_protocol::preamble::PreambleError),
    /// The first framed reply had the wrong message kind.
    UnexpectedWelcome,
    /// Server shut down.
    ServerShutdown {
        reason: Option<shepr_protocol::ShutdownReason>,
    },
    /// Lost connection to the server.
    ConnectionLost(io::Error),
    /// Protocol error (framing, deserialization).
    Protocol(shepr_protocol::FramingError),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::EndpointSetup(err) => {
                write!(f, "failed to set up saved SSH endpoints: {err}")
            }
            ClientError::ConnectionFailed(err) => {
                write!(f, "failed to connect to server: {err}")?;
                write!(
                    f,
                    "\nIs shepr server running? Start it with `shepr server`."
                )
            }
            ClientError::HostTerminal(err) => write!(f, "host terminal error: {err}"),
            ClientError::HandshakeRejected { error } => {
                write!(f, "server rejected handshake: {error}")
            }
            ClientError::Preamble(
                error @ shepr_protocol::preamble::PreambleError::DifferentBuild(_),
            ) => {
                write!(f, "server rejected handshake: {error}")
            }
            ClientError::Preamble(error) => write!(f, "protocol error: {error}"),
            ClientError::UnexpectedWelcome => {
                write!(f, "protocol error: expected endpoint welcome")
            }
            ClientError::ServerShutdown { reason } => {
                write!(f, "server shut down")?;
                if let Some(reason) = reason {
                    write!(f, ": {reason}")?;
                }
                Ok(())
            }
            ClientError::ConnectionLost(err) => write!(f, "lost connection to server: {err}"),
            ClientError::Protocol(err) => write!(f, "protocol error: {err}"),
        }
    }
}

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for ClientError {}

impl From<shepr_protocol::FramingError> for ClientError {
    fn from(err: shepr_protocol::FramingError) -> Self {
        match err {
            shepr_protocol::FramingError::UnexpectedEof => ClientError::ConnectionLost(
                io::Error::new(io::ErrorKind::UnexpectedEof, "server closed connection"),
            ),
            shepr_protocol::FramingError::Io(err) => ClientError::ConnectionLost(err),
            err => ClientError::Protocol(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_lines_carry_the_ending_message() {
        let exit = ClientExit::new(Some("server shut down: updating".into()));
        assert_eq!(
            exit.lines().collect::<Vec<_>>(),
            ["server shut down: updating"]
        );
        let failure = ClientRunError::Session(exit);
        assert_eq!(failure.to_string(), "server shut down: updating");
    }

    #[test]
    fn a_clean_exit_prints_nothing() {
        let exit = ClientExit::new(None);
        assert_eq!(exit.lines().count(), 0);
    }
}
