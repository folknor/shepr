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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseResult {
    Pong {
        version: String,
        build_id: String,
        /// Identifies this server process, which a build id cannot: a
        /// conditional `server.stop` names the boot it expects.
        boot_id: String,
    },
    /// The detector's input for one pane: the detection-source screen text.
    DetectCapture {
        pane_id: PublicPaneId,
        text: String,
    },
    DetectExplain {
        explain: serde_json::Value,
    },
    Ok {},
}
