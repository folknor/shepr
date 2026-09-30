use serde::{Deserialize, Serialize};

use super::common::PaneAgentState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportAgentParams {
    pub pane_id: String,
    /// A built-in source must match the normalized agent label. Other source
    /// names are custom state reporters and cannot own a resume identity.
    pub source: String,
    pub agent: String,
    pub state: PaneAgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Omission retains the current official session identity. An explicitly
    /// supplied invalid official reference fails validation before dispatch.
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportAgentSessionParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_start_source: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_report_ignores_extra_message_without_storing_or_serializing_it() {
        let params: PaneReportAgentParams = serde_json::from_value(serde_json::json!({
            "pane_id": "w1:p1",
            "source": "custom:state",
            "agent": "custom",
            "state": "working",
            "message": "unused integration annotation"
        }))
        .expect("extra JSON fields are accepted");
        let value = serde_json::to_value(params).expect("serialize report");
        assert!(value.get("message").is_none());
    }
}
