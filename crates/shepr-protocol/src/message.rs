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
}

impl std::fmt::Display for HandshakeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpectedHello => f.write_str("expected a handshake as the first message"),
            Self::InvalidSurface(message) => f.write_str(message),
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
    OversizedFrame {
        claimed: usize,
        max: usize,
    },
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
            Self::OversizedFrame { claimed, max } => write!(
                f,
                "The screen is too large to send ({claimed} bytes; the limit is {max}). Make the window smaller; the display resumes once a frame fits."
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

    /// Active-tab pane content rendered at a client-requested origin-relative size.
    PaneSurface(PaneSurfaceFrame),

    /// Immediate endpoint error that the client-rendered shell must show.
    ClientShellError { kind: NoticeKind },

    /// Whether the focused pane needs the shell host to report every key.
    ClientShellKeyboardReportAll { enabled: bool },

    /// One ordered chunk of the final response to an endpoint operation.
    ClientShellEndpointResponseChunk {
        boot_id: BootId,
        request_id: RequestId,
        final_chunk: bool,
        #[serde(
            serialize_with = "codec::serialize_bounded_bytes::<MAX_ENDPOINT_RESPONSE_CHUNK_BYTES, _>",
            deserialize_with = "codec::deserialize_bounded_bytes::<MAX_ENDPOINT_RESPONSE_CHUNK_BYTES, _>"
        )]
        data: Vec<u8>,
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

    /// Client-side result of applying `SurfaceUpdate`, carried in the shared
    /// message pipeline but never sent. Keep it skipped so framing it fails.
    /// Both ends use the same build, so this local variant has no cross-build
    /// wire index to preserve.
    #[serde(skip)]
    PaneSurfacePatch(PaneSurfacePatch),
}
