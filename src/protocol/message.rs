use super::*;
use serde::{Deserialize, Serialize};

/// Terminal ANSI bytes encoded by the server for direct terminal-attach clients.
///
/// The client writes `bytes` straight to stdout and needs nothing else, so the
/// frame carries no sequence number, size or full-redraw flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFrame {
    /// Terminal escape bytes ready to write directly to stdout.
    #[serde(
        serialize_with = "codec::serialize_bounded_bytes::<MAX_FRAME_SIZE, _>",
        deserialize_with = "codec::deserialize_bounded_bytes::<MAX_FRAME_SIZE, _>"
    )]
    pub bytes: Vec<u8>,
}

/// Messages sent from the server to the client over the client protocol socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Direct-terminal handshake response. The hello variant already selects
    /// terminal ANSI frames; errors report why the server rejected the hello.
    Welcome {
        /// If present, the handshake failed and this describes why.
        /// The client should exit with a clear error message.
        error: Option<String>,
    },

    /// Terminal bytes to write directly for a terminal-ANSI client.
    Terminal(TerminalFrame),

    /// Server is shutting down. Clients should exit gracefully.
    ServerShutdown {
        /// Optional reason for the shutdown.
        reason: Option<String>,
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
    ClientShellError { message: String },

    /// Exact Kitty keyboard flags requested by a directly attached terminal.
    /// Zero restores the host terminal's previous keyboard mode.
    DirectTerminalKeyboardProtocol {
        flags: u16,
        modify_other_keys_level: u8,
    },

    /// Whether the focused pane needs the shell host to report every key.
    ClientShellKeyboardReportAll { enabled: bool },

    /// One ordered chunk of the final response to an endpoint operation.
    ClientShellEndpointResponseChunk {
        boot_id: String,
        request_id: String,
        final_chunk: bool,
        #[serde(
            serialize_with = "codec::serialize_bounded_bytes::<MAX_FRAME_SIZE, _>",
            deserialize_with = "codec::deserialize_bounded_bytes::<MAX_FRAME_SIZE, _>"
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
    HealthPong(String),

    /// Something a direct terminal-attach client must tell its user because
    /// the server could not do what the user asked: input dropped because the
    /// pane stopped reading, a paste over the input limit rejected, or a
    /// screen too large to send in one frame. The text is complete and
    /// human-readable; the client shows it as-is.
    ///
    /// The server rate-limits it: a repeating condition (dropped input,
    /// oversized frames) is sent once until it clears, a rejected paste once
    /// per paste. The connection stays up either way, so the client must not
    /// treat this as fatal and must keep the attached terminal usable.
    DirectTerminalNotice { message: String },

    /// Decoded local handoff to the shell. The server sends `SurfaceUpdate`.
    #[serde(skip)]
    PaneSurfacePatch(PaneSurfacePatch),
}
