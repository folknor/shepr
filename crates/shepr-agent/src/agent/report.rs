use super::resume::{AgentSessionRef, PersistedAgentSession, ReportedSessionStart};
use super::{Agent, AgentSource};

/// A report label is either a resolved built-in agent or an open custom name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReportedAgent {
    Known(Agent),
    Custom(String),
}

impl ReportedAgent {
    pub fn parse(label: &str) -> Option<Self> {
        let label = label.trim();
        if label.is_empty() {
            return None;
        }
        Some(
            crate::detect::parse_agent_label(label)
                .map_or_else(|| Self::Custom(label.to_owned()), Self::Known),
        )
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Known(agent) => agent.label(),
            Self::Custom(label) => label,
        }
    }

    pub const fn known(&self) -> Option<Agent> {
        match self {
            Self::Known(agent) => Some(*agent),
            Self::Custom(_) => None,
        }
    }
}

/// Validated ownership shared by state and session reports. Its fields are
/// private so an official source cannot claim a different agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReportOrigin {
    source: AgentSource,
    agent: ReportedAgent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportOriginError {
    EmptyAgent,
    MismatchedAgent,
    UnknownOfficialSource,
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
        let agent = ReportedAgent::parse(label).ok_or(ReportOriginError::EmptyAgent)?;
        Self::new(AgentSource::parse(source), agent)
    }

    fn new(source: AgentSource, agent: ReportedAgent) -> Result<Self, ReportOriginError> {
        if let Some(official) = source.agent() {
            if agent.known() != Some(official) {
                return Err(ReportOriginError::MismatchedAgent);
            }
        } else if source.as_str().starts_with("shepr:") {
            // This namespace belongs to bundled integrations. A misspelled
            // built-in source must not silently become a custom state owner.
            return Err(ReportOriginError::UnknownOfficialSource);
        }
        Ok(Self { source, agent })
    }

    pub fn official(agent: Agent) -> Option<Self> {
        let target = agent.integration_target()?;
        Some(Self {
            source: AgentSource::Official(target),
            agent: ReportedAgent::Known(agent),
        })
    }

    pub fn source(&self) -> &AgentSource {
        &self.source
    }

    pub fn agent(&self) -> &ReportedAgent {
        &self.agent
    }

    pub fn label(&self) -> &str {
        self.agent.label()
    }

    pub const fn known_agent(&self) -> Option<Agent> {
        self.agent.known()
    }

    pub const fn official_agent(&self) -> Option<Agent> {
        self.source.agent()
    }

    pub fn authority_class(&self) -> HookAuthorityClass {
        match self.official_agent() {
            Some(agent) => agent.descriptor().hook_authority_class(),
            None => HookAuthorityClass::PartialState,
        }
    }

    pub fn is_full_lifecycle(&self) -> bool {
        self.authority_class() == HookAuthorityClass::FullLifecycle
    }

    pub fn allows_session_replacement(&self, start: ReportedSessionStart) -> bool {
        self.official_agent().is_some_and(|agent| {
            agent
                .descriptor()
                .hook_session_policy
                .allows_replacement(start)
        })
    }

    pub fn owns(&self, session: &PersistedAgentSession) -> bool {
        self.source == session.source && self.known_agent() == Some(session.agent)
    }

    pub fn session(&self, session_ref: AgentSessionRef) -> Option<PersistedAgentSession> {
        // A custom source naming a built-in agent keeps a resume identity for
        // that agent; an official source's agent is validated at construction.
        PersistedAgentSession::new(self.source.clone(), self.known_agent()?, session_ref)
    }
}

impl super::AgentDescriptor {
    pub const fn hook_authority_class(&self) -> HookAuthorityClass {
        if self.reserves_native_state || self.session_identity_only_integration {
            HookAuthorityClass::SessionOnly
        } else if self.full_lifecycle_hook_authority {
            HookAuthorityClass::FullLifecycle
        } else {
            HookAuthorityClass::PartialState
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_validates_the_reserved_namespace_and_normalizes_labels_once() {
        assert_eq!(
            ReportOrigin::parse("shepr:claud", "claude"),
            Err(ReportOriginError::UnknownOfficialSource)
        );
        assert_eq!(
            ReportOrigin::parse("shepr:claude", "codex"),
            Err(ReportOriginError::MismatchedAgent)
        );
        assert_eq!(
            ReportOrigin::parse("shepr:claude", " Claude ")
                .expect("normalized label")
                .known_agent(),
            Some(Agent::Claude)
        );
        assert!(ReportOrigin::official(Agent::Gemini).is_none());
        assert_eq!(
            ReportOrigin::parse("custom:status", "local bot")
                .expect("custom origin")
                .label(),
            "local bot"
        );
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
