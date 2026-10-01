use super::*;
use serde::{Deserialize, Serialize};

/// Why a client connection ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShutdownReason {
    Message(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeRefusal {
    ExpectedHello,
    InvalidSurface(String),
    /// The server already serves its limit of active client connections,
    /// the value carried. Transient: a connection frees a slot when it ends.
    ConnectionLimit(u32),
}

impl std::fmt::Display for HandshakeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpectedHello => f.write_str("expected a handshake as the first message"),
            Self::InvalidSurface(message) => f.write_str(message),
            Self::ConnectionLimit(limit) => write!(
                f,
                "the server is already serving its limit of {limit} client connections"
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
    PasteRejected {
        size: usize,
        max: usize,
    },
    /// A pane surface encoded past `MAX_MESSAGE_SIZE`, so it cannot be sent
    /// even in parts.
    OversizedSurface {
        claimed: usize,
        max: usize,
    },
}

/// The server's saved session did not come back in full when it started.
/// Carried in every shell snapshot for that server boot, so inactive
/// connections and reconnects retain the same restore diagnosis. It is not a
/// `NoticeKind`: no direct server notice carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRestoreNotice {
    /// Why the session file could not be used at all; `None` when it
    /// loaded and only part of it was discarded.
    pub unusable: Option<String>,
    /// Saved workspaces dropped whole.
    pub dropped_workspaces: usize,
    /// Panes or layout leaves pruned from workspaces that did restore.
    pub panes_pruned: bool,
    /// Where the original session file is kept.
    pub backup_dir: String,
}

impl std::fmt::Display for SessionRestoreNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            unusable,
            dropped_workspaces,
            panes_pruned,
            backup_dir,
        } = self;
        if let Some(reason) = unusable {
            write!(f, "The saved session was not restored: {reason}.")?;
        } else {
            let mut lost = Vec::new();
            if *dropped_workspaces > 0 {
                let unit = if *dropped_workspaces == 1 {
                    "workspace"
                } else {
                    "workspaces"
                };
                lost.push(format!("{dropped_workspaces} saved {unit}"));
            }
            if *panes_pruned {
                lost.push("some saved panes".to_owned());
            }
            write!(
                f,
                "The saved session was restored in part: {} could not be restored.",
                lost.join(" and ")
            )?;
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
            Self::PasteRejected { size, max } => write!(
                f,
                "Paste rejected: Input message is {size} bytes; Shepr's limit is {max} bytes"
            ),
            Self::OversizedSurface { claimed, max } => write!(
                f,
                "The screen is too large to send ({claimed} bytes; the limit is {max}). Make the window smaller; the display resumes once the screen fits."
            ),
        }
    }
}

impl std::fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => f.write_str(message),
        }
    }
}

/// Messages sent from the server to the client over the client protocol socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Server is shutting down. Clients should exit gracefully.
    ServerShutdown {
        /// Optional reason for the shutdown.
        reason: Option<ShutdownReason>,
    },

    /// OSC 52 clipboard data forwarded from a PTY through the server.
    Clipboard {
        /// Base64-encoded clipboard data.
        data: String,
    },

    /// Set the foreground client's outer terminal window title.
    WindowTitle {
        /// Sanitized title to write with OSC 0. `None` restores Shepr's default title.
        title: Option<String>,
    },

    /// Whether the client should currently capture host mouse input.
    MouseCapture {
        /// True when Shepr mouse UI is enabled or the focused pane app requests mouse reporting.
        enabled: bool,
        /// True only while the focused pane requests DEC SGR pixel mode 1016.
        sgr_pixels: bool,
    },

    /// Focused-workspace pane content rendered at a client-requested origin-relative size.
    PaneSurface(PaneSurfaceFrame),

    /// Immediate endpoint error that the client-rendered shell must show.
    ClientShellError { kind: NoticeKind },

    /// Whether the focused pane needs the shell host to report every key.
    ClientShellKeyboardReportAll { enabled: bool },

    /// The one response to a `ClientShellEndpointRequest`. A large result (a
    /// selection copy of a long scrollback) crosses in as many frames as it
    /// needs; only one past `MAX_MESSAGE_SIZE` is answered with an
    /// `EndpointError::ResponseTooLarge` instead.
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
    /// Host presentation effects have crossed the activation fence.
    PresentationReady(String),
    /// Response to a connection health probe.
    HealthPong,
}
