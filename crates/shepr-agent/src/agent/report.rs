use super::resume::{AgentSessionRef, PersistedAgentSession, ReportedSessionStart};
use super::{Agent, AgentSource};

/// Validated ownership shared by state and session reports: a bundled
/// integration's source and the agent it names. The field is private so a
/// report cannot claim a different agent than its source belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReportOrigin {
    source: AgentSource,
}

/// Why a report's source and agent label were refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportOriginError {
    EmptyAgent,
    /// The source is not a bundled integration's.
    UnsupportedSource,
    /// The label does not name the agent the source belongs to.
    MismatchedAgent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAuthorityClass {
    /// Screen detection owns state; hooks may report session identity only.
    SessionOnly,
    PartialState,
    FullLifecycle,
}

impl HookAuthorityClass {
    /// State sent by a session-only integration is interpreted as identity
    /// evidence, just like its session report. It never acquires state authority.
    pub const fn admits_state_report(self) -> bool {
        !matches!(self, Self::SessionOnly)
    }
}

impl ReportOrigin {
    pub fn parse(source: &str, label: &str) -> Result<Self, ReportOriginError> {
        let label = label.trim();
        if label.is_empty() {
            return Err(ReportOriginError::EmptyAgent);
        }
        let source = AgentSource::parse(source).ok_or(ReportOriginError::UnsupportedSource)?;
        if crate::detect::parse_agent_label(label) != Some(source.agent()) {
            return Err(ReportOriginError::MismatchedAgent);
        }
        Ok(Self { source })
    }

    pub fn official(agent: Agent) -> Option<Self> {
        Some(Self {
            source: AgentSource::new(agent.integration_target()?),
        })
    }

    pub fn source(&self) -> &AgentSource {
        &self.source
    }

    pub const fn agent(&self) -> Agent {
        self.source.agent()
    }

    pub const fn authority_class(&self) -> HookAuthorityClass {
        self.source.target().integration().authority_class()
    }

    pub fn is_full_lifecycle(&self) -> bool {
        self.authority_class() == HookAuthorityClass::FullLifecycle
    }

    pub fn allows_session_replacement(&self, start: ReportedSessionStart) -> bool {
        self.agent()
            .descriptor()
            .hook_session_policy()
            .allows_replacement(start)
    }

    pub fn owns(&self, session: &PersistedAgentSession) -> bool {
        self.source == session.source && self.agent() == session.agent
    }

    pub fn session(&self, session_ref: AgentSessionRef) -> Option<PersistedAgentSession> {
        PersistedAgentSession::new(self.source, self.agent(), session_ref)
    }
}

impl super::IntegrationDescriptor {
    /// The authority this integration's reports hold.
    pub const fn authority_class(&self) -> HookAuthorityClass {
        match self.capability {
            super::IntegrationCapability::ScreenOwnedSession
            | super::IntegrationCapability::IdentityOnly => HookAuthorityClass::SessionOnly,
            super::IntegrationCapability::FullLifecycle => HookAuthorityClass::FullLifecycle,
            super::IntegrationCapability::PartialState => HookAuthorityClass::PartialState,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_accepts_only_bundled_sources_and_normalizes_labels_once() {
        assert_eq!(
            ReportOrigin::parse("shepr:claud", "claude"),
            Err(ReportOriginError::UnsupportedSource)
        );
        assert_eq!(
            ReportOrigin::parse("shepr:claude", "codex"),
            Err(ReportOriginError::MismatchedAgent)
        );
        assert_eq!(
            ReportOrigin::parse("shepr:claude", "local bot"),
            Err(ReportOriginError::MismatchedAgent)
        );
        assert_eq!(
            ReportOrigin::parse("shepr:claude", " "),
            Err(ReportOriginError::EmptyAgent)
        );
        assert_eq!(
            ReportOrigin::parse("shepr:claude", " Claude ")
                .expect("normalized label")
                .agent(),
            Agent::Claude
        );
        assert!(ReportOrigin::official(Agent::Gemini).is_none());
        for (source, label) in [
            ("custom:status", "local bot"),
            ("custom:claude", "claude"),
            ("myagent", "myagent"),
            ("", "pi"),
        ] {
            assert_eq!(
                ReportOrigin::parse(source, label),
                Err(ReportOriginError::UnsupportedSource),
                "{source}"
            );
        }
    }

    #[test]
    fn every_session_only_integration_has_the_same_state_admission() {
        for agent in [
            Agent::Claude,
            Agent::Cursor,
            Agent::Devin,
            Agent::GithubCopilot,
            Agent::Droid,
            Agent::Grok,
            Agent::Antigravity,
        ] {
            let origin = ReportOrigin::official(agent).expect("integration");
            assert_eq!(origin.authority_class(), HookAuthorityClass::SessionOnly);
            assert!(!origin.authority_class().admits_state_report());
        }
        assert!(
            ReportOrigin::official(Agent::Codex)
                .expect("Codex")
                .authority_class()
                .admits_state_report()
        );
        assert!(
            ReportOrigin::official(Agent::Pi)
                .expect("Pi")
                .is_full_lifecycle()
        );
    }
}
