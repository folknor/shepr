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
#[expect(
    variant_size_differences,
    reason = "a four-byte limit beside one-byte refusals; boxing it would add an allocation to save three bytes"
)]
pub enum HandshakeRefusal {
    ExpectedHello,
    InvalidSurface(SurfaceRefusal),
    /// The server already serves its limit of active client connections,
    /// the value carried. Transient: a connection frees a slot when it ends.
    ConnectionLimit(u32),
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
    pub loss: SessionRestoreLoss,
    /// Where the original session file is kept.
    pub backup_dir: String,
}

/// What a restore lost. Every variant loses something, so a notice can only
/// exist for a session that did not come back in full.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRestoreLoss {
    /// The session file could not be used at all.
    Unusable { reason: String },
    /// The session file loaded, and these saved workspaces were dropped whole;
    /// `panes_pruned` says whether workspaces that did restore lost panes too.
    Workspaces {
        dropped: std::num::NonZeroUsize,
        panes_pruned: bool,
    },
    /// The session file loaded and every workspace restored, but panes or
    /// layout leaves were pruned from some of them.
    Panes,
}

impl SessionRestoreLoss {
    /// The loss of a session file that loaded, or `None` when nothing in it
    /// was discarded.
    pub fn partial(dropped_workspaces: usize, panes_pruned: bool) -> Option<Self> {
        match std::num::NonZeroUsize::new(dropped_workspaces) {
            Some(dropped) => Some(Self::Workspaces {
                dropped,
                panes_pruned,
            }),
            None => panes_pruned.then_some(Self::Panes),
        }
    }
}

impl std::fmt::Display for SessionRestoreNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { loss, backup_dir } = self;
        let lost = match loss {
            SessionRestoreLoss::Unusable { reason } => {
                write!(f, "The saved session was not restored: {reason}.")?;
                None
            }
            SessionRestoreLoss::Workspaces {
                dropped,
                panes_pruned,
            } => {
                let unit = if dropped.get() == 1 {
                    "workspace"
                } else {
                    "workspaces"
                };
                let workspaces = format!("{dropped} saved {unit}");
                Some(if *panes_pruned {
                    format!("{workspaces} and some saved panes")
                } else {
                    workspaces
                })
            }
            SessionRestoreLoss::Panes => Some("some saved panes".to_owned()),
        };
        if let Some(lost) = lost {
            write!(
                f,
                "The saved session was restored in part: {lost} could not be restored."
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
    fn a_restore_that_lost_nothing_has_no_loss() {
        assert_eq!(SessionRestoreLoss::partial(0, false), None);
        assert_eq!(
            SessionRestoreLoss::partial(0, true),
            Some(SessionRestoreLoss::Panes)
        );
    }

    #[test]
    fn every_loss_names_what_was_lost() {
        let partial = |dropped, pruned| {
            rendered(SessionRestoreLoss::partial(dropped, pruned).expect("a partial loss"))
        };
        assert!(
            partial(1, false).contains("restored in part: 1 saved workspace could not"),
            "{}",
            partial(1, false)
        );
        assert!(
            partial(2, true).contains(": 2 saved workspaces and some saved panes could not"),
            "{}",
            partial(2, true)
        );
        assert!(
            partial(0, true).contains(": some saved panes could not"),
            "{}",
            partial(0, true)
        );
        let unusable = rendered(SessionRestoreLoss::Unusable {
            reason: "it could not be parsed".into(),
        });
        assert!(
            unusable.starts_with("The saved session was not restored: it could not be parsed."),
            "{unusable}"
        );
        assert!(unusable.ends_with("copied to /backups before the server first saves over it."));
    }
}
