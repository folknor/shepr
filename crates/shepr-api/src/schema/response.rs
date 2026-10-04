use serde::{Deserialize, Serialize};

use shepr_protocol::PublicPaneId;

use super::detection::DetectionExplanation;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuccessResponse {
    pub id: String,
    pub result: ResponseResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Absent when a refused or malformed request supplied no unambiguous text ID.
    /// An empty string is a caller-supplied ID and remains `Some`.
    pub id: Option<String>,
    pub error: ErrorBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: crate::error::ApiErrorCode,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: &crate::error::ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.clone(),
            message: message.into(),
        }
    }
}

/// The screen and OSC values the agent detector evaluates for one pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionCapture {
    pub screen: String,
    /// The last OSC title; empty when there was none.
    pub osc_title: String,
    /// The last OSC 9;4 progress report as `4;state[;percent]`; empty when
    /// there was none.
    pub osc_progress: String,
}

impl DetectionCapture {
    /// The OSC title evidence, `None` when the capture holds none.
    pub fn osc_title_evidence(&self) -> Option<&str> {
        (!self.osc_title.is_empty()).then_some(self.osc_title.as_str())
    }

    /// The OSC progress evidence, `None` when the capture holds none.
    pub fn osc_progress_evidence(&self) -> Option<&str> {
        (!self.osc_progress.is_empty()).then_some(self.osc_progress.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseResult {
    Pong {
        version: String,
        build_id: shepr_protocol::BuildIdentity,
        /// Identifies this server process, which a build id cannot: a
        /// conditional `server.stop_if_boot` names the boot it expects.
        boot_id: shepr_protocol::BootId,
        /// The server has begun stopping. Its socket stays up until the final
        /// session save is on disk, so a launcher waits for it to go rather
        /// than attaching to a server that no longer accepts TUI connections. Absent in
        /// a pong from a build that predates it.
        #[serde(default)]
        stopping: bool,
        /// The server has bound its socket but has not finished restoring panes,
        /// and does not yet accept TUI connections. Absent in a pong from a
        /// build that predates it.
        #[serde(default)]
        starting: bool,
    },
    /// The detector's input for one pane, including its OSC title and progress.
    DetectCapture {
        pane_id: PublicPaneId,
        capture: DetectionCapture,
    },
    DetectExplain {
        explain: Box<DetectionExplanation>,
    },
    Ok {},
}
