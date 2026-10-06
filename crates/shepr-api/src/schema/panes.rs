use serde::{Deserialize, Deserializer, Serialize, Serializer};

use shepr_agent::resume::{AgentSessionStartSource, UnrecognizedAgentSessionStartSource};

/// States an integration may report through `pane.report_agent`.
/// `unknown` is a detector/presentation state, not a hook report action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneReportAgentState {
    Working,
    Blocked,
    Idle,
}

/// Params of `pane.report_agent`.
///
/// This is the one API params type that ignores unknown fields. Integration
/// reports may carry extra annotations, but this API only consumes the state
/// and known identity fields and never stores or returns those annotations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportAgentParams {
    pub pane_id: String,
    /// The source of a bundled integration (`shepr:<agent>`). Any other
    /// source is refused with `invalid_agent`.
    pub source: String,
    pub state: PaneReportAgentState,
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
#[serde(deny_unknown_fields)]
pub struct PaneReportAgentSessionParams {
    pub pane_id: String,
    /// The same bundled source validation as a state report.
    pub source: String,
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
    fn state_report_accepts_only_the_integration_state_vocabulary() {
        for state in ["working", "blocked", "idle"] {
            let params = serde_json::from_value::<PaneReportAgentParams>(serde_json::json!({
                "pane_id": "w1:p1",
                "source": "shepr:codex",
                "state": state
            }));
            let params = params.expect("a source-only integration report is accepted");
            let value = serde_json::to_value(params).expect("serialize report");
            assert!(value.get("agent").is_none());
        }

        let unknown = serde_json::from_value::<PaneReportAgentParams>(serde_json::json!({
            "pane_id": "w1:p1",
            "source": "shepr:codex",
            "state": "unknown"
        }));
        assert!(
            unknown.is_err(),
            "unknown is not an integration report state"
        );
    }

    #[test]
    fn state_report_ignores_extra_annotation_without_storing_or_serializing_it() {
        let params: PaneReportAgentParams = serde_json::from_value(serde_json::json!({
            "pane_id": "w1:p1",
            "source": "shepr:codex",
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
            "session_start_source": " resume "
        }))
        .expect("unknown source is retained for server diagnostics");

        let Some(Err(source)) = params.session_start_source else {
            panic!("whitespace is not part of a recognized source spelling");
        };
        assert_eq!(source.as_str(), " resume ");
    }
}
