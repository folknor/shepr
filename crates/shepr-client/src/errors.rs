use std::io;

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

/// What a finished client run leaves for the binary to print.
///
/// By the time a run returns, the client has restored the host terminal,
/// released its SSH resources and flushed its log, so these lines land on the
/// restored screen. The binary writes each one to stderr as `shepr: {line}`;
/// a line may itself span several terminal lines.
#[derive(Debug, Default)]
#[must_use = "the lines carry forwarded notices and reattach guidance the operator must see"]
pub struct ClientExit {
    notices: Vec<String>,
    message: Option<String>,
}

impl ClientExit {
    pub(crate) fn new(notices: Vec<String>, message: Option<String>) -> Self {
        Self { notices, message }
    }

    /// Notices collected while forwarding direct-attach input, then the
    /// message that ended the session, in the order they are to be printed.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.notices
            .iter()
            .map(String::as_str)
            .chain(self.message.as_deref())
    }
}

/// Why a client run failed; the binary prints it and exits nonzero.
#[derive(Debug)]
pub enum ClientRunError {
    /// The client failed while launching. If the host terminal had already
    /// been taken, its guard restored it before this was returned.
    Launch(io::Error),
    /// Loading the saved machine catalog failed before the terminal was taken.
    LaunchCatalog(shepr_remote::machine::CatalogError),
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
            Self::LaunchCatalog(error) => {
                write!(f, "saved SSH endpoint catalog is unavailable: {error}")
            }
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
    UnexpectedWelcome { endpoint: bool },
    /// Server shut down.
    ServerShutdown {
        reason: Option<shepr_protocol::ShutdownReason>,
    },
    /// Lost connection to the server.
    ConnectionLost(io::Error),
    /// Protocol error (framing, deserialization).
    Protocol(shepr_protocol::FramingError),
}

impl ClientError {
    pub(crate) fn display_with_context(&self, context: &ClientErrorContext) -> String {
        match self {
            Self::ServerShutdown {
                reason: Some(shepr_protocol::ShutdownReason::Detached),
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
            ClientError::UnexpectedWelcome { endpoint: true } => {
                write!(f, "protocol error: expected endpoint welcome")
            }
            ClientError::UnexpectedWelcome { endpoint: false } => {
                write!(f, "protocol error: expected Welcome message")
            }
            ClientError::ServerShutdown { reason } => {
                match reason {
                    Some(shepr_protocol::ShutdownReason::Detached) => {
                        write!(f, "detached from server")?;
                    }
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
    fn exit_lines_put_forwarded_notices_before_the_ending_message() {
        let exit = ClientExit::new(
            vec!["first notice".into(), "second notice".into()],
            Some("detached from server\nRun `shepr` to reattach".into()),
        );
        assert_eq!(
            exit.lines().collect::<Vec<_>>(),
            [
                "first notice",
                "second notice",
                "detached from server\nRun `shepr` to reattach"
            ]
        );
        let failure = ClientRunError::Session(exit);
        assert_eq!(
            failure.to_string(),
            "first notice\nsecond notice\ndetached from server\nRun `shepr` to reattach"
        );
    }

    #[test]
    fn a_clean_exit_without_notices_prints_nothing() {
        let exit = ClientExit::new(Vec::new(), None);
        assert_eq!(exit.lines().count(), 0);
    }
}
