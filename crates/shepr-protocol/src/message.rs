use super::*;
use serde::{Deserialize, Serialize};

/// Why a client connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShutdownReason {
    /// This server is stopping and no longer accepts clients.
    Stopping,
}

impl std::fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopping => f.write_str("server is shutting down"),
        }
    }
}

/// Why the server refused the client's requested surface geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceRefusal {
    /// A requested row or column exceeds the terminal dimension limit.
    DimensionTooLarge,
    /// The requested row and column counts exceed the shared cell budget.
    TooManyCells,
    /// A requested pixel dimension for one cell exceeds its safe bound.
    CellTooLarge,
}

impl std::fmt::Display for SurfaceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DimensionTooLarge => {
                f.write_str("client shell pane surface dimensions exceed the dimension limit")
            }
            Self::TooManyCells => {
                f.write_str("client shell pane surface exceeds the surface size limit")
            }
            Self::CellTooLarge => {
                f.write_str("client shell cell pixel size exceeds the safe geometry limit")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeRefusal {
    ExpectedHello,
    InvalidSurface(SurfaceRefusal),
    /// The server already serves its limit of active client connections,
    /// which the carried limit names. Transient: a connection frees a slot
    /// when it ends.
    ConnectionLimit(crate::LimitExceeded),
    /// The server has bound its socket but is still restoring panes.
    /// Transient: it accepts clients once its panes are restored.
    ServerStarting,
}

impl std::fmt::Display for HandshakeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpectedHello => f.write_str("expected a handshake as the first message"),
            Self::InvalidSurface(reason) => write!(f, "{reason}"),
            Self::ServerStarting => f.write_str(
                "the server is still starting; it accepts clients once its panes are restored",
            ),
            Self::ConnectionLimit(error) => write!(
                f,
                "the server is already serving its limit of {} client connections",
                error.limit.max()
            ),
        }
    }
}

impl std::error::Error for HandshakeRefusal {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeKind {
    PaneInputDropped {
        pane_id: PublicPaneId,
        events: usize,
    },
    /// A bounded request or result could not be carried because it exceeded
    /// the named protocol resource limit.
    LimitExceeded(crate::LimitExceeded),
}

/// The server's saved session did not come back exactly as saved when it
/// started: part of it was refused, dropped or repaired.
/// Carried in every shell snapshot for that server boot, so inactive
/// connections and reconnects retain the same restore diagnosis. It is not a
/// `NoticeKind`: no direct server notice carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRestoreNotice {
    pub loss: SessionRestoreLoss,
    /// Where the original session file is kept.
    pub backup_dir: crate::RemotePath,
}

/// Saved data a restore refused, dropped or repaired. Every one of them keeps
/// the source file (backed up before the first save) and is told to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRestoreLoss {
    /// The file could not be decoded or read; no workspace restored.
    Unusable { failure: SessionRestoreFailure },
    /// The file loaded, and restore dropped or repaired parts of it.
    Damaged(SessionRestoreDamage),
}

/// What a restore dropped or repaired in a session file that loaded. At
/// least one part is nonempty (`is_empty` is false) wherever it is carried.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRestoreDamage {
    /// Saved workspaces dropped whole.
    pub dropped_workspaces: usize,
    /// Saved workspaces restored under a fresh ID, their saved one repeated.
    pub renamed_workspaces: usize,
    /// Restored panes whose saved agent session this build cannot use: each
    /// came back as a plain shell, without its session.
    pub dropped_agent_sessions: Vec<crate::PublicPaneId>,
}

impl SessionRestoreDamage {
    pub fn is_empty(&self) -> bool {
        self.dropped_workspaces == 0
            && self.renamed_workspaces == 0
            && self.dropped_agent_sessions.is_empty()
    }

    /// Whether saved data was lost, not only repaired.
    pub fn loses_data(&self) -> bool {
        self.dropped_workspaces > 0 || !self.dropped_agent_sessions.is_empty()
    }
}

impl std::fmt::Display for SessionRestoreDamage {
    /// One sentence per kind of damage, separated by spaces.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut sentences = Vec::new();
        if self.dropped_workspaces > 0 {
            let unit = if self.dropped_workspaces == 1 {
                "workspace"
            } else {
                "workspaces"
            };
            sentences.push(format!(
                "The saved session was restored in part: {} saved {unit} could not be restored.",
                self.dropped_workspaces
            ));
        }
        if !self.dropped_agent_sessions.is_empty() {
            let panes = self
                .dropped_agent_sessions
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            sentences.push(if self.dropped_agent_sessions.len() == 1 {
                format!(
                    "The saved agent session of pane {panes} could not be used, so the pane came back as a plain shell."
                )
            } else {
                format!(
                    "The saved agent sessions of panes {panes} could not be used, so those panes came back as plain shells."
                )
            });
        }
        if self.renamed_workspaces > 0 {
            let unit = if self.renamed_workspaces == 1 {
                "workspace ID was"
            } else {
                "workspace IDs were"
            };
            sentences.push(format!(
                "The saved session needed repair: {} duplicate {unit} reassigned.",
                self.renamed_workspaces
            ));
        }
        f.write_str(&sentences.join(" "))
    }
}

/// Why a saved session file could not be used. `detail` preserves the
/// filesystem or schema diagnostic shown to the user; the variant and parse
/// coordinates keep the outcome machine-readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRestoreFailure {
    /// Reading the file failed for this operating-system reason.
    Unreadable {
        kind: SessionIoErrorKind,
        detail: String,
    },
    /// The path resolved to a directory, special file or other non-regular object.
    NotRegularFile {
        kind: SessionFileKind,
        detail: String,
    },
    /// The file exceeded the reader's byte limit.
    TooLarge { limit_bytes: usize },
    /// JSON decoding or schema validation failed at this location.
    Unparseable {
        line: usize,
        column: usize,
        category: SessionParseCategory,
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionIoErrorKind {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    NotConnected,
    AddrInUse,
    AddrNotAvailable,
    BrokenPipe,
    WouldBlock,
    InvalidInput,
    InvalidData,
    ResourceBusy,
    TimedOut,
    Interrupted,
    Unsupported,
    UnexpectedEof,
    OutOfMemory,
    WriteZero,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionFileKind {
    Directory,
    Fifo,
    Socket,
    CharacterDevice,
    BlockDevice,
    Other,
}

/// serde_json's high-level category for a session parse failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionParseCategory {
    Io,
    Syntax,
    Data,
    Eof,
}

impl std::fmt::Display for SessionRestoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable { detail, .. } | Self::NotRegularFile { detail, .. } => {
                write!(f, "it could not be read: {detail}")
            }
            Self::TooLarge { limit_bytes } => {
                write!(f, "it exceeds the {limit_bytes}-byte session file limit")
            }
            Self::Unparseable { detail, .. } => {
                write!(f, "it could not be parsed: {detail}")
            }
        }
    }
}

impl std::fmt::Display for SessionRestoreNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { loss, backup_dir } = self;
        match loss {
            SessionRestoreLoss::Unusable { failure } => {
                write!(f, "The saved session was not restored: {failure}.")?;
            }
            SessionRestoreLoss::Damaged(damage) => write!(f, "{damage}")?,
        }
        write!(
            f,
            " The original session file is copied to {backup_dir} before the server first saves over it."
        )
    }
}

impl std::fmt::Display for NoticeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PaneInputDropped { pane_id, events } => {
                let unit = if *events == 1 { "event" } else { "events" };
                write!(
                    f,
                    "Input to pane {pane_id} dropped ({events} {unit}): the pane is not reading its input"
                )
            }
            Self::LimitExceeded(error) => match error.limit.kind() {
                crate::LimitKind::InputPayloadBytes => write!(
                    f,
                    "Paste rejected: Input message is {} bytes; Shepr's limit is {} bytes",
                    error.actual,
                    error.limit.max()
                ),
                crate::LimitKind::SurfaceMessageBytes => write!(
                    f,
                    "The screen is too large to send ({} bytes; the limit is {}). Make the window smaller; the display resumes once the screen fits.",
                    error.actual,
                    error.limit.max()
                ),
                _ => write!(f, "{error}"),
            },
        }
    }
}

/// Messages sent from the server to the client over the client protocol socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Server is shutting down. Clients should exit gracefully.
    ServerShutdown {
        /// Why the server is stopping.
        reason: ShutdownReason,
    },

    /// OSC 52 clipboard data forwarded from a PTY through the server.
    Clipboard {
        /// Bytes decoded from OSC 52 and capped by terminal parsing before this message is
        /// created. The framed reader also caps the complete server message at
        /// [`MAX_MESSAGE_SIZE`], so this byte buffer does not need
        /// the codec's collection-item limit.
        #[serde(
            serialize_with = "codec::serialize_byte_vec",
            deserialize_with = "codec::deserialize_byte_vec"
        )]
        data: Vec<u8>,
    },

    /// Whether the client should currently capture host mouse input.
    MouseCapture {
        /// `Cells` when Shepr mouse UI is enabled or the focused pane app requests mouse
        /// reporting; `Pixels` only when this client may also address that pane in pixels.
        mode: shepr_term::mouse::HostMouseCapture,
    },

    /// Focused-workspace pane content rendered at a client-requested origin-relative size.
    PaneSurface(PaneSurfaceFrame),

    /// Immediate endpoint error that the client-rendered shell must show.
    ClientShellError { kind: NoticeKind },

    /// Whether the focused pane needs the shell host to report every key.
    ClientShellKeyboardReportAll { enabled: bool },

    /// The one response to a `ClientShellEndpointRequest`. A large result (a
    /// selection copy of a long scrollback) crosses in as many frames as it
    /// needs; a response past `MAX_MESSAGE_SIZE` gets a typed size-limit error.
    ClientShellEndpointResponse {
        boot_id: BootId,
        request_id: RequestId,
        result: Result<crate::command::EndpointReply, crate::command::EndpointError>,
    },

    /// Incremental cells and optional projection metadata against one full surface.
    SurfaceUpdate(SurfaceUpdate),

    /// Response to a client-owned shell hello.
    EndpointWelcome(super::endpoint::EndpointServerWelcome),
    /// Current client-owned shell projection.
    EndpointSnapshot(Box<ClientShellSnapshot>),
    /// Response to a connection health probe.
    HealthPong,
}

impl ServerMessage {
    /// The terminal notice sent when this server stops accepting clients.
    pub fn server_shutdown() -> Self {
        Self::ServerShutdown {
            reason: ShutdownReason::Stopping,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(loss: SessionRestoreLoss) -> String {
        SessionRestoreNotice {
            loss,
            backup_dir: "/backups".into(),
        }
        .to_string()
    }

    #[test]
    fn every_loss_names_what_was_lost() {
        let pane = |number: usize| {
            crate::PublicPaneId::new(
                &crate::WorkspaceId::from_number(1).expect("nonzero workspace number"),
                crate::PanePublicNumber::new(number).expect("nonzero"),
            )
        };
        let partial = |dropped_workspaces, renamed_workspaces, dropped_agent_sessions| {
            rendered(SessionRestoreLoss::Damaged(SessionRestoreDamage {
                dropped_workspaces,
                renamed_workspaces,
                dropped_agent_sessions,
            }))
        };
        assert!(partial(1, 0, vec![]).contains("restored in part: 1 saved workspace could not"));
        assert!(partial(2, 1, vec![]).contains("2 saved workspaces could not"));
        assert!(partial(2, 1, vec![]).contains(
            ". The saved session needed repair: 1 duplicate workspace ID was reassigned."
        ));
        assert!(partial(0, 2, vec![]).starts_with(
            "The saved session needed repair: 2 duplicate workspace IDs were reassigned."
        ));
        assert!(!partial(0, 2, vec![]).contains("pane"));
        let sessions = partial(0, 0, vec![pane(1), pane(2)]);
        assert!(
            sessions.starts_with(&format!(
                "The saved agent sessions of panes {}, {} could not be used",
                pane(1),
                pane(2)
            )),
            "{sessions}"
        );
        assert!(partial(0, 0, vec![pane(1)]).contains(&format!("of pane {} could", pane(1))));
        let unusable = rendered(SessionRestoreLoss::Unusable {
            failure: SessionRestoreFailure::Unparseable {
                line: 1,
                column: 2,
                category: SessionParseCategory::Syntax,
                detail: "expected a value at line 1 column 2".into(),
            },
        });
        assert!(
            unusable.starts_with(
                "The saved session was not restored: it could not be parsed: expected a value at line 1 column 2."
            ),
            "{unusable}"
        );
        assert!(unusable.ends_with("copied to /backups before the server first saves over it."));
    }
}
