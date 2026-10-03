//! Typed diagnostic payloads for screen rules and hook authority.

use serde::{Deserialize, Serialize};
use shepr_agent::AgentState;
use shepr_detect::manifest::{DetectionExplain, FallbackReason, SkippedUpdateReason};
use shepr_detect::ownership::HookRejection;

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
    /// The pane's last hook report that was parked or rejected, while no
    /// later report from that source has applied. Always absent for a saved
    /// capture evaluated with `--file`, which has no pane.
    pub last_unapplied_hook_report: Option<UnappliedHookReport>,
}

/// A hook report that changed nothing, and why. The hook itself was answered
/// with success: reports are fire-and-forget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnappliedHookReport {
    pub hook_source: String,
    pub agent: String,
    pub report: UnappliedHookReportKind,
    pub seq: Option<u64>,
    pub session_ref: Option<shepr_agent::resume::AgentSessionRef>,
    /// The server's wall clock when it admitted the report, in milliseconds
    /// since the Unix epoch.
    pub received_unix_ms: u64,
    /// How long before the explain request the report was admitted, by the
    /// server's monotonic clock.
    pub age_ms: u64,
    pub outcome: UnappliedHookOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnappliedHookReportKind {
    State {
        state: DetectionState,
    },
    SessionStart {
        /// Absent when the report carried no start source.
        start_source: Option<ReportedStartSource>,
    },
}

/// The start source a session start report carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedStartSource {
    Startup,
    Resume,
    Clear,
    Compact,
    New,
    Load,
    Fork,
    Select,
    /// A start source this build does not know.
    Unrecognized,
}

impl ReportedStartSource {
    fn from_reported(start: shepr_agent::resume::ReportedSessionStart) -> Option<Self> {
        use shepr_agent::resume::{AgentSessionStartSource, ReportedSessionStart};
        Some(match start {
            ReportedSessionStart::Omitted => return None,
            ReportedSessionStart::Unrecognized => Self::Unrecognized,
            ReportedSessionStart::Known(source) => match source {
                AgentSessionStartSource::Startup => Self::Startup,
                AgentSessionStartSource::Resume => Self::Resume,
                AgentSessionStartSource::Clear => Self::Clear,
                AgentSessionStartSource::Compact => Self::Compact,
                AgentSessionStartSource::New => Self::New,
                AgentSessionStartSource::Load => Self::Load,
                AgentSessionStartSource::Fork => Self::Fork,
                AgentSessionStartSource::Select => Self::Select,
            },
        })
    }
}

impl std::fmt::Display for ReportedStartSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
            Self::Clear => "clear",
            Self::Compact => "compact",
            Self::New => "new",
            Self::Load => "load",
            Self::Fork => "fork",
            Self::Select => "select",
            Self::Unrecognized => "unrecognized",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnappliedHookOutcome {
    /// Held until process evidence for its agent arrives.
    Parked,
    Rejected {
        reason: HookRejection,
    },
}

impl UnappliedHookReport {
    /// The payload for `report`, aged against `now` on the server's
    /// monotonic clock.
    pub fn from_ownership(
        report: &shepr_detect::ownership::UnappliedHookReport,
        now: std::time::Instant,
    ) -> Self {
        use shepr_detect::ownership::{HookReportKind, UnappliedHookDisposition};
        let millis =
            |duration: std::time::Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        Self {
            hook_source: report.origin.source().as_str().to_owned(),
            agent: report.origin.agent().label().to_owned(),
            report: match report.kind {
                HookReportKind::State(state) => UnappliedHookReportKind::State {
                    state: state.into(),
                },
                HookReportKind::SessionStart(start) => UnappliedHookReportKind::SessionStart {
                    start_source: ReportedStartSource::from_reported(start),
                },
            },
            seq: report.seq,
            session_ref: report.session_ref.clone(),
            received_unix_ms: report
                .received
                .wall
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, millis),
            age_ms: millis(now.saturating_duration_since(report.received.monotonic)),
            outcome: match report.disposition {
                UnappliedHookDisposition::Parked => UnappliedHookOutcome::Parked,
                UnappliedHookDisposition::Rejected(reason) => {
                    UnappliedHookOutcome::Rejected { reason }
                }
            },
        }
    }
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
            last_unapplied_hook_report: None,
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
            last_unapplied_hook_report: None,
        }
    }

    /// Attaches the pane's last unapplied hook report, if it has one.
    pub fn with_last_unapplied_hook_report(mut self, report: Option<UnappliedHookReport>) -> Self {
        self.last_unapplied_hook_report = report;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_and_hook_explanations_round_trip_as_typed_payloads() {
        let screen: DetectionExplanation = shepr_detect::manifest::explain_for_label(
            "codex",
            shepr_detect::manifest::DetectionInput {
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

    fn unapplied(
        kind: shepr_detect::ownership::HookReportKind,
        disposition: shepr_detect::ownership::UnappliedHookDisposition,
        received: std::time::Instant,
    ) -> shepr_detect::ownership::UnappliedHookReport {
        shepr_detect::ownership::UnappliedHookReport {
            origin: shepr_agent::ReportOrigin::parse("shepr:codex", "codex").expect("test origin"),
            kind,
            seq: Some(12),
            session_ref: shepr_agent::resume::AgentSessionRef::id("codex-session"),
            received: shepr_detect::ownership::HookClockSample {
                monotonic: received,
                wall: std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123),
            },
            disposition,
        }
    }

    #[test]
    fn unapplied_hook_reports_have_a_pinned_json_shape() {
        let received = std::time::Instant::now();
        let now = received + std::time::Duration::from_millis(1_500);
        let rejected = UnappliedHookReport::from_ownership(
            &unapplied(
                shepr_detect::ownership::HookReportKind::State(AgentState::Working),
                shepr_detect::ownership::UnappliedHookDisposition::Rejected(
                    HookRejection::OutOfOrder,
                ),
                received,
            ),
            now,
        );
        assert_eq!(
            serde_json::to_value(&rejected).expect("encode report"),
            serde_json::json!({
                "hook_source": "shepr:codex",
                "agent": "codex",
                "report": { "kind": "state", "state": "working" },
                "seq": 12,
                "session_ref": { "kind": "id", "value": "codex-session" },
                "received_unix_ms": 1_700_000_000_123_u64,
                "age_ms": 1_500,
                "outcome": { "kind": "rejected", "reason": "out_of_order" },
            })
        );

        let parked = UnappliedHookReport::from_ownership(
            &unapplied(
                shepr_detect::ownership::HookReportKind::SessionStart(
                    shepr_agent::resume::ReportedSessionStart::Known(
                        shepr_agent::resume::AgentSessionStartSource::Startup,
                    ),
                ),
                shepr_detect::ownership::UnappliedHookDisposition::Parked,
                received,
            ),
            now,
        );
        let json = serde_json::to_value(&parked).expect("encode report");
        assert_eq!(
            json["report"],
            serde_json::json!({ "kind": "session_start", "start_source": "startup" })
        );
        assert_eq!(json["outcome"], serde_json::json!({ "kind": "parked" }));

        let omitted = UnappliedHookReport::from_ownership(
            &unapplied(
                shepr_detect::ownership::HookReportKind::SessionStart(
                    shepr_agent::resume::ReportedSessionStart::Omitted,
                ),
                shepr_detect::ownership::UnappliedHookDisposition::Parked,
                received,
            ),
            now,
        );
        assert_eq!(
            omitted.report,
            UnappliedHookReportKind::SessionStart { start_source: None }
        );

        let explain = DetectionExplanation::hook_authority(
            "codex",
            AgentState::Working,
            "shepr:codex",
            ScreenDetectionSkipReason::HookAuthority,
        )
        .with_last_unapplied_hook_report(Some(rejected));
        let decoded: DetectionExplanation =
            serde_json::from_str(&serde_json::to_string(&explain).expect("encode explanation"))
                .expect("decode explanation");
        assert_eq!(decoded, explain);
    }

    #[test]
    fn every_rejection_reason_displays_as_its_json_spelling() {
        for reason in [
            HookRejection::MissingSession,
            HookRejection::InvalidSession,
            HookRejection::ReplacedSession,
            HookRejection::ProcessExited,
            HookRejection::DetectedAgentConflict,
            HookRejection::OwnerConflict,
            HookRejection::LifecycleGate,
            HookRejection::RetiredSession,
            HookRejection::CrossTalk,
            HookRejection::UnrecognizedStart,
            HookRejection::MissingSequence,
            HookRejection::OutOfOrder,
            HookRejection::ProcessRequired,
        ] {
            assert_eq!(
                serde_json::to_value(reason).expect("encode reason"),
                serde_json::Value::String(reason.to_string())
            );
        }
    }

    #[test]
    fn unknown_agent_label_has_a_closed_fallback_reason() {
        let explain: DetectionExplanation = shepr_detect::manifest::explain_for_label(
            "not-yet-known",
            shepr_detect::manifest::DetectionInput {
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
