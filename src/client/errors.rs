use std::io;

use crate::protocol;

/// All environment and target details needed to present a client failure.
pub(crate) struct ClientErrorContext {
    remote_reattach: Option<String>,
    local_reattach: String,
}

impl ClientErrorContext {
    pub(crate) fn new(local_reattach: String, remote_reattach: Option<String>) -> Self {
        Self {
            remote_reattach,
            local_reattach,
        }
    }
}

/// Errors that can occur during client operation.
#[derive(Debug)]
pub enum ClientError {
    /// Could not connect to the server's client socket.
    ConnectionFailed(io::Error),
    /// A host terminal write failed while updating terminal modes or output.
    HostTerminal(io::Error),
    /// Server rejected our handshake.
    HandshakeRejected { error: protocol::HandshakeRefusal },
    /// The peer did not send a valid build preamble.
    Preamble(protocol::preamble::PreambleError),
    /// The first framed reply had the wrong message kind.
    UnexpectedWelcome { endpoint: bool },
    /// A delta bypassed connection-local surface decoding.
    SurfaceUpdateBeforeDecode,
    /// Server shut down.
    ServerShutdown {
        reason: Option<protocol::ShutdownReason>,
    },
    /// Lost connection to the server.
    ConnectionLost(io::Error),
    /// Protocol error (framing, deserialization).
    Protocol(protocol::FramingError),
}

impl ClientError {
    pub(crate) fn display_with_context(&self, context: &ClientErrorContext) -> String {
        match self {
            Self::ServerShutdown {
                reason: Some(protocol::ShutdownReason::Detached),
            } => {
                if let Some(command) = &context.remote_reattach {
                    format!("detached from remote server\nRun `{command}` to reattach")
                } else {
                    format!(
                        "detached from server\nRun `{}` to reattach",
                        context.local_reattach
                    )
                }
            }
            Self::ConnectionLost(error) if let Some(command) = &context.remote_reattach => {
                format!(
                    "lost connection to remote Shepr: {error}\nIf the remote server survived the SSH or network drop, its panes may still be running.\nRun `{command}` to reattach"
                )
            }
            _ => self.to_string(),
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
            ClientError::Preamble(error @ protocol::preamble::PreambleError::DifferentBuild(_)) => {
                write!(f, "server rejected handshake: {error}")
            }
            ClientError::Preamble(error) => write!(f, "protocol error: {error}"),
            ClientError::UnexpectedWelcome { endpoint: true } => {
                write!(f, "protocol error: expected endpoint welcome")
            }
            ClientError::UnexpectedWelcome { endpoint: false } => {
                write!(f, "protocol error: expected Welcome message")
            }
            ClientError::SurfaceUpdateBeforeDecode => write!(
                f,
                "protocol error: surface update reached presentation before decoding"
            ),
            ClientError::ServerShutdown { reason } => {
                match reason {
                    Some(protocol::ShutdownReason::Detached) => write!(f, "detached from server")?,
                    _ => {
                        write!(f, "server shut down")?;
                        if let Some(reason) = reason {
                            write!(f, ": {reason}")?;
                        }
                    }
                }
                Ok(())
            }
            ClientError::ConnectionLost(err) => write!(f, "lost connection to server: {err}"),
            ClientError::Protocol(err) => write!(f, "protocol error: {err}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::ConnectionFailed(err) => Some(err),
            ClientError::HostTerminal(err) => Some(err),
            ClientError::ConnectionLost(err) => Some(err),
            ClientError::Protocol(err) => Some(err),
            ClientError::Preamble(err) => Some(err),
            _ => None,
        }
    }
}

impl From<protocol::FramingError> for ClientError {
    fn from(err: protocol::FramingError) -> Self {
        match err {
            protocol::FramingError::UnexpectedEof => ClientError::ConnectionLost(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed connection",
            )),
            protocol::FramingError::Io(err) => ClientError::ConnectionLost(err),
            err => ClientError::Protocol(err),
        }
    }
}
