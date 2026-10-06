use crate::resume::{AgentSessionRef, PersistedAgentSession, ReportedSessionStart};
use crate::{Agent, AgentSource};

/// Validated ownership shared by state and session reports: a bundled
/// integration's source, from which its agent is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReportOrigin {
    source: AgentSource,
}

/// Why a report's source was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportOriginError {
    /// The source is not a bundled integration's.
    UnsupportedSource,
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
    pub fn parse(source: &str) -> Result<Self, ReportOriginError> {
        let source = AgentSource::parse(source).ok_or(ReportOriginError::UnsupportedSource)?;
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
        self.source == session.source
    }

    pub fn session(&self, session_ref: AgentSessionRef) -> Option<PersistedAgentSession> {
        PersistedAgentSession::new(self.source, session_ref)
    }
}

impl crate::IntegrationDescriptor {
    /// The authority this integration's reports hold.
    pub const fn authority_class(&self) -> HookAuthorityClass {
        match self.capability {
            crate::IntegrationCapability::ScreenOwnedSession
            | crate::IntegrationCapability::IdentityOnly => HookAuthorityClass::SessionOnly,
            crate::IntegrationCapability::FullLifecycle => HookAuthorityClass::FullLifecycle,
            crate::IntegrationCapability::PartialState => HookAuthorityClass::PartialState,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_accepts_only_exact_bundled_sources() {
        for source in [
            "shepr:claud",
            "custom:status",
            "custom:claude",
            "myagent",
            "",
            " shepr:claude ",
            "shepr:Claude",
        ] {
            assert_eq!(
                ReportOrigin::parse(source),
                Err(ReportOriginError::UnsupportedSource),
                "{source}",
            );
        }
        assert_eq!(
            ReportOrigin::parse("shepr:claude")
                .expect("official source")
                .agent(),
            Agent::Claude,
        );
        assert!(ReportOrigin::official(Agent::Gemini).is_none());
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
