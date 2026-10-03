//! Typed diagnostic payloads for screen rules and hook authority.

use serde::{Deserialize, Serialize};
use shepr_agent::detect::AgentState;
use shepr_agent::detect::manifest::{DetectionExplain, FallbackReason, SkippedUpdateReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl From<AgentState> for DetectionState {
    fn from(state: AgentState) -> Self {
        match state {
            AgentState::Idle => Self::Idle,
            AgentState::Working => Self::Working,
            AgentState::Blocked => Self::Blocked,
            AgentState::Unknown => Self::Unknown,
        }
    }
}

impl std::fmt::Display for DetectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenDetectionSkipReason {
    HookAuthority,
    FullLifecycleHookAuthority,
}

impl std::fmt::Display for ScreenDetectionSkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::HookAuthority => "hook_authority",
            Self::FullLifecycleHookAuthority => "full_lifecycle_hook_authority",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DetectionStateSource {
    Screen,
    HookAuthority {
        hook_source: String,
        skip_reason: ScreenDetectionSkipReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionExplanation {
    pub agent: String,
    pub state: DetectionState,
    pub state_source: DetectionStateSource,
    pub matched_rule: Option<DetectionMatchedRule>,
    pub visible_idle: bool,
    pub visible_blocker: bool,
    pub visible_working: bool,
    pub skip_state_update: bool,
    pub skipped_update_reason: Option<SkippedUpdateReason>,
    pub fallback_reason: Option<FallbackReason>,
    pub evaluated_rules: Vec<DetectionEvaluatedRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionMatchedRule {
    pub id: String,
    pub priority: i32,
    pub region: String,
    pub state: DetectionState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionEvaluatedRule {
    pub id: String,
    pub priority: i32,
    pub region: String,
    pub state: DetectionState,
    pub matched: bool,
    pub evidence: DetectionRuleEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionRuleEvidence {
    pub contains: Vec<String>,
    pub regex: Vec<String>,
    pub line_regex: Vec<String>,
    pub all_count: usize,
    pub any_count: usize,
    pub not_count: usize,
    pub region_bytes: usize,
    pub region_preview: String,
}

impl From<DetectionExplain> for DetectionExplanation {
    fn from(explain: DetectionExplain) -> Self {
        Self {
            agent: explain.agent.label().to_owned(),
            state: explain.verdict.state().into(),
            state_source: DetectionStateSource::Screen,
            matched_rule: explain.matched_rule.map(|rule| DetectionMatchedRule {
                id: rule.id,
                priority: rule.priority,
                region: rule.region.to_string(),
                state: rule.state.into(),
            }),
            visible_idle: explain.verdict.visible_idle(),
            visible_blocker: explain.verdict.visible_blocker(),
            visible_working: explain.verdict.visible_working(),
            skip_state_update: explain.verdict.skip_state_update(),
            skipped_update_reason: explain.skipped_update_reason,
            fallback_reason: explain.fallback_reason,
            evaluated_rules: explain
                .evaluated_rules
                .into_iter()
                .map(|rule| DetectionEvaluatedRule {
                    id: rule.id,
                    priority: rule.priority,
                    region: rule.region.to_string(),
                    state: rule.state.into(),
                    matched: rule.matched,
                    evidence: DetectionRuleEvidence {
                        contains: rule.evidence.contains,
                        regex: rule.evidence.regex,
                        line_regex: rule.evidence.line_regex,
                        all_count: rule.evidence.all_count,
                        any_count: rule.evidence.any_count,
                        not_count: rule.evidence.not_count,
                        region_bytes: rule.evidence.region_bytes,
                        region_preview: rule.evidence.region_preview,
                    },
                })
                .collect(),
        }
    }
}

impl DetectionExplanation {
    pub fn hook_authority(
        agent: &str,
        state: AgentState,
        hook_source: &str,
        skip_reason: ScreenDetectionSkipReason,
    ) -> Self {
        Self {
            agent: agent.to_owned(),
            state: state.into(),
            state_source: DetectionStateSource::HookAuthority {
                hook_source: hook_source.to_owned(),
                skip_reason,
            },
            matched_rule: None,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            skip_state_update: false,
            skipped_update_reason: None,
            fallback_reason: None,
            evaluated_rules: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_and_hook_explanations_round_trip_as_typed_payloads() {
        let screen: DetectionExplanation = shepr_agent::detect::manifest::explain_for_label(
            "codex",
            shepr_agent::detect::manifest::DetectionInput {
                screen: "press enter to confirm or esc to cancel",
                osc_title: "",
                osc_progress: "",
            },
        )
        .into();
        let hook = DetectionExplanation::hook_authority(
            "omp",
            AgentState::Working,
            "shepr:omp",
            ScreenDetectionSkipReason::FullLifecycleHookAuthority,
        );
        for explain in [screen, hook] {
            let json = serde_json::to_string(&explain).expect("encode explanation");
            let decoded: DetectionExplanation =
                serde_json::from_str(&json).expect("decode explanation");
            assert_eq!(decoded, explain);
        }
    }

    #[test]
    fn unknown_agent_label_has_a_closed_fallback_reason() {
        let explain: DetectionExplanation = shepr_agent::detect::manifest::explain_for_label(
            "not-yet-known",
            shepr_agent::detect::manifest::DetectionInput {
                screen: "",
                osc_title: "",
                osc_progress: "",
            },
        )
        .into();
        assert_eq!(explain.agent, "not-yet-known");
        assert_eq!(explain.fallback_reason, Some(FallbackReason::UnknownAgent));
        let mut json = serde_json::to_value(&explain).expect("encode explanation");
        json["fallback_reason"] = serde_json::json!("misspelled_reason");
        assert!(serde_json::from_value::<DetectionExplanation>(json).is_err());
    }
}
