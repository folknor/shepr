use serde::{Deserialize, Deserializer, Serialize, Serializer};

use shepr_agent::agent::resume::{AgentSessionStartSource, UnrecognizedAgentSessionStartSource};

use super::common::PaneAgentState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportAgentParams {
    pub pane_id: String,
    /// The source of a bundled integration (`shepr:<agent>`). Any other
    /// source is refused with `invalid_agent`.
    pub source: String,
    /// The agent the source belongs to, resolved and validated with source
    /// before internal dispatch; a label naming another agent is refused.
    pub agent: String,
    pub state: PaneAgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Omission retains the current official session identity. An explicitly
    /// supplied invalid official reference is rejected by the app handler.
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Mutually exclusive with agent_session_id; both supplied is a bad request.
    pub agent_session_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportAgentSessionParams {
    pub pane_id: String,
    /// The same source and agent validation as a state report.
    pub source: String,
    /// Resolved and validated with source before internal dispatch.
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Mutually exclusive with agent_session_id; both supplied is a bad request.
    pub agent_session_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(with = "session_start_source_wire")]
    pub session_start_source:
        Option<Result<AgentSessionStartSource, UnrecognizedAgentSessionStartSource>>,
}

mod session_start_source_wire {
    use super::*;

    pub(super) fn serialize<S>(
        source: &Option<Result<AgentSessionStartSource, UnrecognizedAgentSessionStartSource>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = source.as_ref().map(|source| match source {
            Ok(source) => source.as_str(),
            Err(source) => source.as_str(),
        });
        match value {
            Some(value) => serializer.serialize_some(value),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<
        Option<Result<AgentSessionStartSource, UnrecognizedAgentSessionStartSource>>,
        D::Error,
    >
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)
            .map(|value| value.map(|value| AgentSessionStartSource::parse(&value)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_report_ignores_extra_message_without_storing_or_serializing_it() {
        let params: PaneReportAgentParams = serde_json::from_value(serde_json::json!({
            "pane_id": "w1:p1",
            "source": "shepr:codex",
            "agent": "codex",
            "state": "working",
            "message": "unused integration annotation"
        }))
        .expect("extra JSON fields are accepted");
        let value = serde_json::to_value(params).expect("serialize report");
        assert!(value.get("message").is_none());
    }

    #[test]
    fn session_report_retains_unknown_start_source_as_a_typed_error() {
        let params: PaneReportAgentSessionParams = serde_json::from_value(serde_json::json!({
            "pane_id": "w1:p1",
            "source": "shepr:pi",
            "agent": "pi",
            "session_start_source": "future-source"
        }))
        .expect("unknown source is retained for server diagnostics");

        let Some(Err(source)) = params.session_start_source.as_ref() else {
            panic!("unknown source is retained as a parse error");
        };
        assert_eq!(source.as_str(), "future-source");
        let value = serde_json::to_value(params).expect("serialize session report");
        assert_eq!(value["session_start_source"], "future-source");
    }

    #[test]
    fn session_report_source_parsing_does_not_trim_values() {
        let params: PaneReportAgentSessionParams = serde_json::from_value(serde_json::json!({
            "pane_id": "w1:p1",
            "source": "shepr:pi",
            "agent": "pi",
            "session_start_source": " resume "
        }))
        .expect("unknown source is retained for server diagnostics");

        let Some(Err(source)) = params.session_start_source else {
            panic!("whitespace is not part of a recognized source spelling");
        };
        assert_eq!(source.as_str(), " resume ");
    }
}
