use std::fmt;
use std::hash::Hash;
use std::path::Path;

use serde::{Deserialize, Serialize, de::Visitor};

use crate::limits::{MAX_SESSION_ID_LEN, MAX_SESSION_PATH_LEN};

use super::{Agent, AgentSource, ResumeArgs, SessionRefPolicy};

pub use shepr_core::agent_session::AgentSessionRefKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentSessionStartSource {
    Startup,
    Resume,
    Clear,
    Compact,
    New,
    Load,
    Fork,
    Select,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionId(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct AbsoluteSessionPath(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum AgentSessionRef {
    Id(SessionId),
    Path(AbsoluteSessionPath),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentResumeKey {
    source: AgentSource,
    agent: Agent,
    session_ref: AgentSessionRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumePlan {
    pub source: AgentSource,
    pub agent: Agent,
    pub argv: Vec<String>,
    pub dedupe_key: AgentResumeKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedAgentSession {
    pub source: AgentSource,
    pub agent: Agent,
    pub session_ref: AgentSessionRef,
}

impl SessionId {
    fn new(value: String) -> Option<Self> {
        valid_session_id(&value).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AbsoluteSessionPath {
    fn new(value: String) -> Option<Self> {
        valid_session_path(&value).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AgentSessionRef {
    pub fn id(value: impl Into<String>) -> Option<Self> {
        Some(Self::Id(SessionId::new(value.into())?))
    }

    pub fn path(value: impl Into<String>) -> Option<Self> {
        Some(Self::Path(AbsoluteSessionPath::new(value.into())?))
    }

    pub const fn kind(&self) -> AgentSessionRefKind {
        match self {
            Self::Id(_) => AgentSessionRefKind::Id,
            Self::Path(_) => AgentSessionRefKind::Path,
        }
    }

    pub fn value(&self) -> String {
        self.value_str().to_owned()
    }

    pub fn value_str(&self) -> &str {
        match self {
            Self::Id(session) => session.as_str(),
            Self::Path(path) => path.as_str(),
        }
    }

    fn accepted_for(&self, agent: Agent) -> bool {
        let Some(policy) = agent
            .descriptor()
            .resume_support
            .map(|support| support.session_ref_policy)
        else {
            return false;
        };
        match (policy, self) {
            (SessionRefPolicy::Id, Self::Id(_))
            | (SessionRefPolicy::IdOrPath, Self::Id(_) | Self::Path(_)) => true,
            (SessionRefPolicy::Id, Self::Path(_)) => false,
        }
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct IdVisitor;
        impl Visitor<'_> for IdVisitor {
            type Value = SessionId;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a valid agent session ID")
            }
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                SessionId::new(value.to_owned())
                    .ok_or_else(|| E::custom("invalid agent session ID"))
            }
        }
        deserializer.deserialize_str(IdVisitor)
    }
}

impl<'de> Deserialize<'de> for AbsoluteSessionPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct PathVisitor;
        impl Visitor<'_> for PathVisitor {
            type Value = AbsoluteSessionPath;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an absolute agent session path")
            }
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                AbsoluteSessionPath::new(value.to_owned())
                    .ok_or_else(|| E::custom("invalid agent session path"))
            }
        }
        deserializer.deserialize_str(PathVisitor)
    }
}

impl PersistedAgentSession {
    pub fn new(source: AgentSource, agent: Agent, session_ref: AgentSessionRef) -> Option<Self> {
        (source
            .agent()
            .is_none_or(|source_agent| source_agent == agent)
            && session_ref.accepted_for(agent))
        .then_some(Self {
            source,
            agent,
            session_ref,
        })
    }

    pub fn from_report(
        source: &str,
        agent_label: &str,
        session_ref: AgentSessionRef,
    ) -> Option<Self> {
        let agent = Agent::parse_canonical_label(agent_label)?;
        let source = AgentSource::parse(source);
        Self::new(source, agent, session_ref)
    }
}

impl AgentResumePlan {
    fn with_argv(session: &PersistedAgentSession, argv: Vec<String>) -> Option<Self> {
        (session.source == AgentSource::Official(session.agent)
            && session.session_ref.accepted_for(session.agent))
        .then(|| Self {
            source: session.source.clone(),
            agent: session.agent,
            argv,
            dedupe_key: AgentResumeKey {
                source: session.source.clone(),
                agent: session.agent,
                session_ref: session.session_ref.clone(),
            },
        })
    }
}

pub fn session_ref_from_report(
    source: &str,
    agent_label: &str,
    agent_session_id: Option<String>,
    agent_session_path: Option<String>,
) -> Option<AgentSessionRef> {
    let source = AgentSource::from_pair(source, agent_label)?;
    let agent = source.agent()?;
    let policy = agent.descriptor().resume_support?.session_ref_policy;
    match (policy, agent_session_path, agent_session_id) {
        (SessionRefPolicy::IdOrPath, Some(path), agent_session_id) => {
            AgentSessionRef::path(path).or_else(|| agent_session_id.and_then(AgentSessionRef::id))
        }
        (SessionRefPolicy::IdOrPath, None, Some(id)) | (SessionRefPolicy::Id, _, Some(id)) => {
            AgentSessionRef::id(id)
        }
        _ => None,
    }
}

pub fn session_ref_from_snapshot(
    source: &AgentSource,
    agent: Agent,
    session_ref: &AgentSessionRef,
) -> Option<PersistedAgentSession> {
    PersistedAgentSession::new(source.clone(), agent, session_ref.clone())
}

pub fn plan(session: &PersistedAgentSession) -> Option<AgentResumePlan> {
    let agent = session.agent;
    let descriptor = agent.descriptor();
    if session.source != AgentSource::Official(agent) || !session.session_ref.accepted_for(agent) {
        return None;
    }

    let executable = agent.executable().to_owned();
    let argv = match (descriptor.resume_support?.resume_args, &session.session_ref) {
        (ResumeArgs::FlagValue(flag), reference) => {
            vec![executable, flag.to_owned(), reference.value()]
        }
        (ResumeArgs::InlineFlag(flag), reference) => {
            vec![executable, format!("{flag}{}", reference.value())]
        }
        (ResumeArgs::Subcommand(subcommand), reference) => {
            vec![executable, subcommand.to_owned(), reference.value()]
        }
    };
    AgentResumePlan::with_argv(session, argv)
}

impl AgentSessionStartSource {
    pub(crate) const ALL: [Self; 8] = [
        Self::Startup,
        Self::Resume,
        Self::Clear,
        Self::Compact,
        Self::New,
        Self::Load,
        Self::Fork,
        Self::Select,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
            Self::Clear => "clear",
            Self::Compact => "compact",
            Self::New => "new",
            Self::Load => "load",
            Self::Fork => "fork",
            Self::Select => "select",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        Self::ALL
            .iter()
            .copied()
            .find(|source| source.as_str() == value)
    }
}

pub fn normalize_session_start_source(value: Option<&str>) -> Option<AgentSessionStartSource> {
    value.and_then(AgentSessionStartSource::parse)
}

pub fn is_reserved_native_state_source(source: &str, agent_label: &str) -> bool {
    AgentSource::from_pair(source, agent_label)
        .and_then(|source| source.agent())
        .is_some_and(|agent| agent.descriptor().reserves_native_state)
}

pub fn is_official_agent_source(source: &str, agent_label: &str) -> bool {
    AgentSource::from_pair(source, agent_label).is_some()
}

// An ID is passed as one argument after a resume flag. Reject leading dashes
// and control characters at construction and again when deserializing.
fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_ID_LEN
        && !value.starts_with('-')
        && !value.chars().any(char::is_control)
}

fn valid_session_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_PATH_LEN
        && !value.chars().any(char::is_control)
        && Path::new(value).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_for_labels(
        source: &str,
        agent_label: &str,
        session_ref: &AgentSessionRef,
    ) -> Option<AgentResumePlan> {
        let source = AgentSource::from_pair(source, agent_label)?;
        let agent = source.agent()?;
        let session = PersistedAgentSession::new(source, agent, session_ref.clone())?;
        plan(&session)
    }

    fn snapshot_session_for_labels(
        source: &str,
        agent_label: &str,
        kind: AgentSessionRefKind,
        value: &str,
    ) -> Option<PersistedAgentSession> {
        let source = AgentSource::from_pair(source, agent_label)?;
        let agent = source.agent()?;
        let session_ref = match kind {
            AgentSessionRefKind::Id => AgentSessionRef::id(value)?,
            AgentSessionRefKind::Path => AgentSessionRef::path(value.to_owned())?,
        };
        PersistedAgentSession::new(source, agent, session_ref)
    }

    fn absolute_test_path(name: &str) -> String {
        // Planner tests validate these references but never open the paths.
        format!("/shepr-agent-test/{name}")
    }

    #[test]
    fn native_state_reservation_excludes_full_lifecycle_sources() {
        assert!(is_reserved_native_state_source("shepr:claude", "claude"));
        assert!(!is_reserved_native_state_source("shepr:codex", "codex"));
        assert!(is_reserved_native_state_source("shepr:devin", "devin"));
        assert!(!is_reserved_native_state_source("shepr:kimi", "kimi"));
        assert!(!is_reserved_native_state_source(
            "shepr:opencode",
            "opencode"
        ));
    }

    #[test]
    fn planner_allows_supported_agents() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        let omp_session = absolute_test_path("omp-session.jsonl");
        assert_eq!(
            plan_for_labels(
                "shepr:claude",
                "claude",
                &AgentSessionRef::id("claude-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["claude", "--resume", "claude-session"]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:codex",
                "codex",
                &AgentSessionRef::id("codex-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["codex", "resume", "codex-session"]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:copilot",
                "copilot",
                &AgentSessionRef::id("copilot-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["copilot", "--resume=copilot-session"]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:devin",
                "devin",
                &AgentSessionRef::id("devin-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["devin", "--resume", "devin-session"]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:droid",
                "droid",
                &AgentSessionRef::id("droid-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["droid", "--resume", "droid-session"]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:kimi",
                "kimi",
                &AgentSessionRef::id("kimi-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["kimi", "--session", "kimi-session"]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:mastracode",
                "mastracode",
                &AgentSessionRef::id("mastracode-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["mastracode", "--thread", "mastracode-session"]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:pi",
                "pi",
                &AgentSessionRef::path(&pi_session).expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["pi", "--session", pi_session.as_str()]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:omp",
                "omp",
                &AgentSessionRef::path(&omp_session).expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["omp", format!("--resume={omp_session}").as_str()]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:opencode",
                "opencode",
                &AgentSessionRef::id("opencode-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["opencode", "--session", "opencode-session"]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:kilo",
                "kilo",
                &AgentSessionRef::id("kilo-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["kilo", "--session", "kilo-session"]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:cursor",
                "cursor",
                &AgentSessionRef::id("cursor-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["cursor-agent", "--resume", "cursor-session",]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:agy",
                "agy",
                &AgentSessionRef::id("agy-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["agy", "--conversation", "agy-session"]
        );
        assert_eq!(
            plan_for_labels(
                "shepr:grok",
                "grok",
                &AgentSessionRef::id("grok-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["grok", "--resume", "grok-session"]
        );
    }

    #[test]
    fn planner_rejects_custom_and_unsupported_path_refs() {
        let claude_session = absolute_test_path("claude-session");
        assert!(
            plan_for_labels(
                "custom:claude",
                "claude",
                &AgentSessionRef::id("session").expect("test precondition")
            )
            .is_none()
        );
        assert!(
            plan_for_labels(
                "shepr:claude",
                "claude",
                &AgentSessionRef::path(&claude_session).expect("test precondition")
            )
            .is_none()
        );
    }

    #[test]
    fn report_ref_prefers_pi_and_omp_paths_and_validates_values() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        let omp_session = absolute_test_path("omp-session.jsonl");
        let claude_session = absolute_test_path("claude-session");
        let copilot_session = absolute_test_path("copilot-session");
        let session_ref = session_ref_from_report(
            "shepr:pi",
            "pi",
            Some("pi-id".into()),
            Some(pi_session.clone()),
        )
        .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Path);
        assert_eq!(session_ref.value_str(), pi_session);

        assert!(session_ref_from_report("shepr:pi", "pi", Some("bad\nid".into()), None).is_none());
        assert!(
            session_ref_from_report("shepr:pi", "pi", None, Some("relative.jsonl".into()))
                .is_none()
        );
        assert!(session_ref_from_report("custom:pi", "pi", Some("pi-id".into()), None).is_none());

        let session_ref = session_ref_from_report(
            "shepr:omp",
            "omp",
            Some("omp-id".into()),
            Some(omp_session.clone()),
        )
        .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Path);
        assert_eq!(session_ref.value_str(), omp_session);

        let session_ref = session_ref_from_report("shepr:omp", "omp", Some("omp-id".into()), None)
            .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "omp-id");
        let session_ref = session_ref_from_report(
            "shepr:omp",
            "omp",
            Some("omp-id".into()),
            Some("relative.jsonl".into()),
        )
        .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "omp-id");
        assert!(
            session_ref_from_report("shepr:omp", "omp", None, Some("relative.jsonl".into()))
                .is_none()
        );

        assert!(
            session_ref_from_report("shepr:claude", "claude", None, Some(claude_session)).is_none()
        );

        let session_ref =
            session_ref_from_report("shepr:copilot", "copilot", Some("copilot-id".into()), None)
                .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "copilot-id");
        assert!(
            session_ref_from_report("shepr:copilot", "copilot", None, Some(copilot_session))
                .is_none()
        );

        let session_ref =
            session_ref_from_report("shepr:devin", "devin", Some("devin-id".into()), None)
                .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "devin-id");

        let session_ref =
            session_ref_from_report("shepr:droid", "droid", Some("droid-id".into()), None)
                .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "droid-id");
        assert!(
            session_ref_from_report(
                "shepr:droid",
                "droid",
                None,
                Some("/tmp/droid-session".into())
            )
            .is_none()
        );

        let session_ref =
            session_ref_from_report("shepr:kimi", "kimi", Some("kimi-id".into()), None)
                .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "kimi-id");

        let session_ref = session_ref_from_report(
            "shepr:mastracode",
            "mastracode",
            Some("mastracode-id".into()),
            None,
        )
        .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "mastracode-id");

        let session_ref =
            session_ref_from_report("shepr:kilo", "kilo", Some("kilo-id".into()), None)
                .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "kilo-id");

        for (source, label) in [
            ("shepr:qodercli", "qodercli"),
            ("shepr:qwen", "qwen"),
            ("shepr:letta", "letta"),
        ] {
            assert!(session_ref_from_report(source, label, Some("id".into()), None).is_none());
        }

        let session_ref = session_ref_from_report("shepr:agy", "agy", Some("agy-id".into()), None)
            .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "agy-id");
    }

    #[test]
    fn normalize_session_start_source_allows_known_values() {
        assert_eq!(
            normalize_session_start_source(Some("startup")),
            Some(AgentSessionStartSource::Startup)
        );
        assert_eq!(
            normalize_session_start_source(Some("resume")),
            Some(AgentSessionStartSource::Resume)
        );
        assert_eq!(
            normalize_session_start_source(Some("clear")),
            Some(AgentSessionStartSource::Clear)
        );
        assert_eq!(
            normalize_session_start_source(Some("compact")),
            Some(AgentSessionStartSource::Compact)
        );
        assert_eq!(
            normalize_session_start_source(Some("new")),
            Some(AgentSessionStartSource::New)
        );
        assert_eq!(
            normalize_session_start_source(Some("load")),
            Some(AgentSessionStartSource::Load)
        );
        assert_eq!(
            normalize_session_start_source(Some("fork")),
            Some(AgentSessionStartSource::Fork)
        );
        assert_eq!(
            normalize_session_start_source(Some("select")),
            Some(AgentSessionStartSource::Select)
        );
        assert_eq!(
            normalize_session_start_source(Some(" resume ")),
            Some(AgentSessionStartSource::Resume)
        );
        assert_eq!(normalize_session_start_source(Some("other")), None);
        assert_eq!(normalize_session_start_source(None), None);
    }

    #[test]
    fn every_session_start_source_round_trips_through_its_string() {
        for source in AgentSessionStartSource::ALL {
            assert_eq!(
                normalize_session_start_source(Some(source.as_str())),
                Some(source)
            );
        }
    }

    #[test]
    fn ids_are_data_not_shell_text() {
        let id = "abc; rm -rf /";
        let codex_plan = plan_for_labels(
            "shepr:codex",
            "codex",
            &AgentSessionRef::id(id).expect("test precondition"),
        )
        .expect("test precondition");
        assert_eq!(codex_plan.argv, vec!["codex", "resume", id]);

        let copilot_plan = plan_for_labels(
            "shepr:copilot",
            "copilot",
            &AgentSessionRef::id(id).expect("test precondition"),
        )
        .expect("test precondition");
        assert_eq!(copilot_plan.argv, vec!["copilot", "--resume=abc; rm -rf /"]);

        let devin_plan = plan_for_labels(
            "shepr:devin",
            "devin",
            &AgentSessionRef::id(id).expect("test precondition"),
        )
        .expect("test precondition");
        assert_eq!(devin_plan.argv, vec!["devin", "--resume", id]);
    }

    #[test]
    fn ids_that_read_as_flags_are_rejected_on_every_path() {
        for id in ["-", "--dangerously-skip-permissions", "-x"] {
            assert!(AgentSessionRef::id(id).is_none(), "{id}");
            assert!(
                session_ref_from_report("shepr:claude", "claude", Some(id.into()), None).is_none()
            );
            assert!(
                snapshot_session_for_labels("shepr:claude", "claude", AgentSessionRefKind::Id, id)
                    .is_none()
            );
        }
        // A value only has to avoid a leading dash; dashes inside are fine.
        assert!(AgentSessionRef::id("abc-def").is_some());
    }

    #[test]
    fn session_reference_deserialization_revalidates_values() {
        for value in ["--dangerously-skip-permissions", "bad\nid"] {
            let encoded = serde_json::to_string(value).expect("test precondition");
            let json = format!(r#"{{"kind":"id","value":{encoded}}}"#);
            assert!(
                serde_json::from_str::<AgentSessionRef>(&json).is_err(),
                "{value:?}"
            );
        }
        assert!(AgentSessionRef::id("--dangerously-skip-permissions").is_none());
        assert!(AgentSessionRef::path("relative-session.jsonl").is_none());
        assert!(AgentSessionRef::id("abc-def").is_some());
    }

    #[test]
    fn planner_rejects_path_refs_for_id_only_agents() {
        let opencode_session = absolute_test_path("opencode-session");
        let kilo_session = absolute_test_path("kilo-session");
        let copilot_session = absolute_test_path("copilot-session");
        let devin_session = absolute_test_path("devin-session");
        assert!(
            plan_for_labels(
                "shepr:opencode",
                "opencode",
                &AgentSessionRef::path(&opencode_session).expect("test precondition")
            )
            .is_none()
        );
        assert!(
            plan_for_labels(
                "shepr:kilo",
                "kilo",
                &AgentSessionRef::path(&kilo_session).expect("test precondition")
            )
            .is_none()
        );
        assert!(
            plan_for_labels(
                "shepr:copilot",
                "copilot",
                &AgentSessionRef::path(&copilot_session).expect("test precondition")
            )
            .is_none()
        );
        assert!(
            plan_for_labels(
                "shepr:devin",
                "devin",
                &AgentSessionRef::path(&devin_session).expect("test precondition")
            )
            .is_none()
        );
        assert!(
            snapshot_session_for_labels(
                "shepr:mastracode",
                "mastracode",
                AgentSessionRefKind::Id,
                "mastracode-session"
            )
            .is_some()
        );
        assert!(
            snapshot_session_for_labels(
                "shepr:opencode",
                "opencode",
                AgentSessionRefKind::Id,
                "opencode-session"
            )
            .is_some()
        );
        assert!(
            snapshot_session_for_labels(
                "shepr:kilo",
                "kilo",
                AgentSessionRefKind::Id,
                "kilo-session"
            )
            .is_some()
        );
        assert!(
            snapshot_session_for_labels(
                "shepr:copilot",
                "copilot",
                AgentSessionRefKind::Id,
                "copilot-session"
            )
            .is_some()
        );
        assert!(
            snapshot_session_for_labels(
                "shepr:devin",
                "devin",
                AgentSessionRefKind::Id,
                "devin-session"
            )
            .is_some()
        );
        assert!(
            snapshot_session_for_labels("shepr:agy", "agy", AgentSessionRefKind::Id, "agy-session")
                .is_some()
        );
        let agy_session = absolute_test_path("agy-session");
        assert!(
            plan_for_labels(
                "shepr:agy",
                "agy",
                &AgentSessionRef::path(&agy_session).expect("test precondition")
            )
            .is_none()
        );
    }
}
