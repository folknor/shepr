use serde::{Deserialize, Serialize};

use shepr_protocol::PublicPaneId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuccessResponse {
    pub id: String,
    pub result: ResponseResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub id: String,
    pub error: ErrorBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: &crate::error::ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_owned(),
            message: message.into(),
        }
    }
}

/// The screen and OSC values the agent detector evaluates for one pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionCapture {
    pub screen: String,
    pub osc_title: String,
    pub osc_progress: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseResult {
    Pong {
        version: String,
        build_id: String,
        /// Identifies this server process, which a build id cannot: a
        /// conditional `server.stop_if_boot` names the boot it expects.
        boot_id: String,
        /// The server has begun stopping. Its sockets stay up until the final
        /// session save is on disk, so a launcher waits for them to go rather
        /// than attaching to a client socket that no longer accepts. Absent in
        /// a pong from a build that predates it.
        #[serde(default)]
        stopping: bool,
    },
    /// The detector's input for one pane, including its OSC title and progress.
    DetectCapture {
        pane_id: PublicPaneId,
        capture: DetectionCapture,
    },
    DetectExplain {
        explain: serde_json::Value,
    },
    Ok {},
}
