use std::fmt;
use std::hash::Hash;
use std::path::Path;

use serde::{Deserialize, Serialize, de::Visitor};

use crate::limits::{MAX_SESSION_ID_LEN, MAX_SESSION_PATH_LEN};

use crate::{Agent, AgentSource, ResumeArgs, SessionRefPolicy};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnrecognizedAgentSessionStartSource(String);

impl UnrecognizedAgentSessionStartSource {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UnrecognizedAgentSessionStartSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unrecognized agent session start source {:?}",
            self.0
        )
    }
}

impl std::error::Error for UnrecognizedAgentSessionStartSource {}

/// What a session report says about how its session started, as the
/// replacement policy reads it.
///
/// An unrecognized source is its own case, neither omitted nor dropped. Most
/// assets forward the agent's own start value verbatim, and an agent can start
/// sending a new value at any time, ahead of a shepr build that knows it.
/// Treating it as omitted would hand it the no-source replacement rule
/// (`replace_without_start`), so an unknown start could replace a live
/// session. Dropping the whole report would lose the session identity, and
/// with it resume on restore, for every session of that agent until shepr
/// ships an update. An unrecognized start therefore records an identity when
/// nothing conflicts, and never counts as a replacement or as a confirmed
/// start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportedSessionStart {
    /// The report carried no start source.
    Omitted,
    Known(AgentSessionStartSource),
    /// The report carried a start source this build does not know.
    Unrecognized,
}

impl ReportedSessionStart {
    /// Classifies a wire value; `None` is a report without a source.
    pub fn from_wire(value: Option<&str>) -> Self {
        match value.map(AgentSessionStartSource::parse) {
            None => Self::Omitted,
            Some(Ok(source)) => Self::Known(source),
            Some(Err(_)) => Self::Unrecognized,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionId(String);

// Resume paths are UTF-8 command arguments reported by agent hooks. Retain
// the exact text, including redundant separators; a PathBuf would still need
// a UTF-8 check whenever the planner borrows the command argument.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct AbsoluteSessionPath(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum AgentSessionRef {
    Id(SessionId),
    Path(AbsoluteSessionPath),
}

/// A resume key is the saved identity itself, without another copy of its fields.
pub type AgentResumeKey = PersistedAgentSession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumePlan {
    session: PersistedAgentSession,
    argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct PersistedAgentSession {
    pub(crate) source: AgentSource,
    pub(crate) session_ref: AgentSessionRef,
}

impl<'de> Deserialize<'de> for PersistedAgentSession {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SavedSession {
            source: AgentSource,
            session_ref: AgentSessionRef,
        }
        let saved = SavedSession::deserialize(deserializer)?;
        Self::new(saved.source, saved.session_ref)
            .ok_or_else(|| serde::de::Error::custom("invalid saved agent session"))
    }
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

    pub const fn is_id(&self) -> bool {
        matches!(self, Self::Id(_))
    }

    pub const fn kind(&self) -> AgentSessionRefKind {
        match self {
            Self::Id(_) => AgentSessionRefKind::Id,
            Self::Path(_) => AgentSessionRefKind::Path,
        }
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
    /// The reference must be one the source's agent can resume.
    fn is_valid_identity(&self) -> bool {
        !self.agent().executable().is_empty() && self.session_ref.accepted_for(self.agent())
    }

    pub fn source(&self) -> &AgentSource {
        &self.source
    }

    pub const fn agent(&self) -> Agent {
        self.source.agent()
    }

    pub fn session_ref(&self) -> &AgentSessionRef {
        &self.session_ref
    }

    pub fn new(source: AgentSource, session_ref: AgentSessionRef) -> Option<Self> {
        let session = Self {
            source,
            session_ref,
        };
        session.is_valid_identity().then_some(session)
    }

    pub fn from_report(source: &str, session_ref: AgentSessionRef) -> Option<Self> {
        let source = AgentSource::parse(source)?;
        Self::new(source, session_ref)
    }

    /// Build the resume command for this validated identity. Construction
    /// admits only agents with resume support and a nonempty executable, so
    /// planning cannot fail after a session has been accepted.
    pub fn resume_plan(&self) -> AgentResumePlan {
        let agent = self.agent();
        let descriptor = agent.descriptor();
        let resume_args = descriptor
            .resume_support
            .expect("persisted agent sessions require resume support")
            .resume_args;
        let executable = agent.executable().to_owned();
        let argv = match (resume_args, &self.session_ref) {
            (ResumeArgs::FlagValue(flag), reference) => {
                vec![
                    executable,
                    flag.to_owned(),
                    reference.value_str().to_owned(),
                ]
            }
            (ResumeArgs::InlineFlag(flag), reference) => {
                vec![executable, format!("{flag}{}", reference.value_str())]
            }
            (ResumeArgs::Subcommand(subcommand), reference) => vec![
                executable,
                subcommand.to_owned(),
                reference.value_str().to_owned(),
            ],
        };
        AgentResumePlan::from_argv(self, argv)
    }
}

impl AgentResumePlan {
    fn from_argv(session: &PersistedAgentSession, argv: Vec<String>) -> Self {
        Self {
            session: session.clone(),
            argv,
        }
    }

    fn with_argv(session: &PersistedAgentSession, argv: Vec<String>) -> Option<Self> {
        argv.first()
            .is_some_and(|program| !program.is_empty())
            .then(|| Self::from_argv(session, argv))
    }

    pub fn agent(&self) -> Agent {
        self.session.agent()
    }

    pub fn key(&self) -> &AgentResumeKey {
        &self.session
    }

    /// The arguments after the executable, which the constructor requires.
    pub fn args(&self) -> &[String] {
        &self.argv[1..]
    }

    pub fn to_shell_command(&self) -> String {
        shepr_core::shell_quote::join_argv(&self.argv)
    }

    /// Construct an explicit command for this resume identity. The executable
    /// must be nonempty, just as for commands derived from agent descriptors.
    pub fn for_command(
        session: &PersistedAgentSession,
        program: String,
        args: Vec<String>,
    ) -> Option<Self> {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(program);
        argv.extend(args);
        Self::with_argv(session, argv)
    }
}

/// Decode an official report after its source has been validated.
/// The API can retain its parsed agent instead of resolving the source again.
pub fn session_ref_for_agent_report(
    agent: Agent,
    agent_session_id: Option<String>,
    agent_session_path: Option<String>,
) -> Option<AgentSessionRef> {
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

    pub fn parse(value: &str) -> Result<Self, UnrecognizedAgentSessionStartSource> {
        Self::ALL
            .iter()
            .copied()
            .find(|source| source.as_str() == value)
            .ok_or_else(|| UnrecognizedAgentSessionStartSource(value.to_owned()))
    }
}

// Like the agent table, the variant list must cover the enum in declaration
// order. The exhaustive match makes a new variant require classification, and
// its true arm must be the last declared variant and end the list.
const _: () = {
    const fn is_last_variant(source: AgentSessionStartSource) -> bool {
        match source {
            AgentSessionStartSource::Select => true,
            AgentSessionStartSource::Startup
            | AgentSessionStartSource::Resume
            | AgentSessionStartSource::Clear
            | AgentSessionStartSource::Compact
            | AgentSessionStartSource::New
            | AgentSessionStartSource::Load
            | AgentSessionStartSource::Fork => false,
        }
    }
    let all = AgentSessionStartSource::ALL;
    assert!(is_last_variant(all[all.len() - 1]));
    let mut index = 0;
    while index < all.len() {
        assert!(all[index] as usize == index);
        index += 1;
    }
};

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

    fn session_ref_from_report(
        source: &str,
        agent_session_id: Option<String>,
        agent_session_path: Option<String>,
    ) -> Option<AgentSessionRef> {
        let source = AgentSource::parse(source)?;
        session_ref_for_agent_report(source.agent(), agent_session_id, agent_session_path)
    }

    fn plan_for_source(source: &str, session_ref: &AgentSessionRef) -> Option<AgentResumePlan> {
        let source = AgentSource::parse(source)?;
        let session = PersistedAgentSession::new(source, session_ref.clone())?;
        Some(session.resume_plan())
    }

    #[test]
    fn saved_sessions_validate_source_and_reference() {
        for (source, kind, value) in [
            ("shepr:claude", "path", "/sessions/claude"),
            ("shepr:removed-agent", "id", "session"),
            ("custom:codex", "id", "session"),
        ] {
            let saved = serde_json::json!({
                "source": source,
                "session_ref": { "kind": kind, "value": value },
            });
            assert!(serde_json::from_value::<PersistedAgentSession>(saved).is_err());
        }
        let session = PersistedAgentSession::from_report(
            "shepr:codex",
            AgentSessionRef::id("session").expect("session ID"),
        )
        .expect("official session");
        assert_eq!(session.agent(), Agent::Codex);
        let saved = serde_json::to_value(&session).expect("encode session");
        assert!(saved.get("agent").is_none());
        assert_eq!(
            serde_json::from_value::<PersistedAgentSession>(saved.clone()).expect("decode session"),
            session,
        );
        let mut duplicate = saved;
        duplicate["agent"] = serde_json::json!("codex");
        assert!(serde_json::from_value::<PersistedAgentSession>(duplicate).is_err());
    }

    #[test]
    fn a_plan_with_nothing_to_run_is_refused() {
        let session = PersistedAgentSession::new(
            AgentSource::new(crate::IntegrationTarget::Codex),
            AgentSessionRef::id("abc").expect("test precondition"),
        )
        .expect("test precondition");
        assert!(AgentResumePlan::with_argv(&session, Vec::new()).is_none());
        assert!(AgentResumePlan::with_argv(&session, vec![String::new()]).is_none());
        assert!(AgentResumePlan::with_argv(&session, vec!["codex".into()]).is_some());
    }

    fn snapshot_session_for_source(
        source: &str,
        kind: AgentSessionRefKind,
        value: &str,
    ) -> Option<PersistedAgentSession> {
        let source = AgentSource::parse(source)?;
        let session_ref = match kind {
            AgentSessionRefKind::Id => AgentSessionRef::id(value)?,
            AgentSessionRefKind::Path => AgentSessionRef::path(value.to_owned())?,
        };
        PersistedAgentSession::new(source, session_ref)
    }

    fn absolute_test_path(name: &str) -> String {
        // Planner tests validate these references but never open the paths.
        format!("/shepr-agent-test/{name}")
    }

    #[test]
    fn planner_allows_supported_agents() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        let omp_session = absolute_test_path("omp-session.jsonl");
        assert_eq!(
            plan_for_source(
                "shepr:claude",
                &AgentSessionRef::id("claude-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["claude", "--resume", "claude-session"]
        );
        assert_eq!(
            plan_for_source(
                "shepr:codex",
                &AgentSessionRef::id("codex-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["codex", "resume", "codex-session"]
        );
        assert_eq!(
            plan_for_source(
                "shepr:copilot",
                &AgentSessionRef::id("copilot-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["copilot", "--resume=copilot-session"]
        );
        assert_eq!(
            plan_for_source(
                "shepr:devin",
                &AgentSessionRef::id("devin-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["devin", "--resume", "devin-session"]
        );
        assert_eq!(
            plan_for_source(
                "shepr:droid",
                &AgentSessionRef::id("droid-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["droid", "--resume", "droid-session"]
        );
        assert_eq!(
            plan_for_source(
                "shepr:kimi",
                &AgentSessionRef::id("kimi-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["kimi", "--session", "kimi-session"]
        );
        assert_eq!(
            plan_for_source(
                "shepr:mastracode",
                &AgentSessionRef::id("mastracode-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["mastracode", "--thread", "mastracode-session"]
        );
        assert_eq!(
            plan_for_source(
                "shepr:pi",
                &AgentSessionRef::path(&pi_session).expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["pi", "--session", pi_session.as_str()]
        );
        assert_eq!(
            plan_for_source(
                "shepr:omp",
                &AgentSessionRef::path(&omp_session).expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["omp", format!("--resume={omp_session}").as_str()]
        );
        assert_eq!(
            plan_for_source(
                "shepr:opencode",
                &AgentSessionRef::id("opencode-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["opencode", "--session", "opencode-session"]
        );
        assert_eq!(
            plan_for_source(
                "shepr:kilo",
                &AgentSessionRef::id("kilo-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["kilo", "--session", "kilo-session"]
        );
        assert_eq!(
            plan_for_source(
                "shepr:cursor",
                &AgentSessionRef::id("cursor-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["cursor-agent", "--resume", "cursor-session",]
        );
        assert_eq!(
            plan_for_source(
                "shepr:agy",
                &AgentSessionRef::id("agy-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["agy", "--conversation", "agy-session"]
        );
        assert_eq!(
            plan_for_source(
                "shepr:grok",
                &AgentSessionRef::id("grok-session").expect("test precondition")
            )
            .expect("test precondition")
            .argv,
            vec!["grok", "--resume", "grok-session"]
        );
    }

    #[test]
    fn planner_rejects_unbundled_sources_and_unsupported_path_refs() {
        let claude_session = absolute_test_path("claude-session");
        assert!(
            plan_for_source(
                "custom:claude",
                &AgentSessionRef::id("session").expect("test precondition")
            )
            .is_none()
        );
        assert!(
            plan_for_source(
                "shepr:claude",
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
        let session_ref =
            session_ref_from_report("shepr:pi", Some("pi-id".into()), Some(pi_session.clone()))
                .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Path);
        assert_eq!(session_ref.value_str(), pi_session);

        assert!(session_ref_from_report("shepr:pi", Some("bad\nid".into()), None).is_none());
        assert!(session_ref_from_report("shepr:pi", None, Some("relative.jsonl".into())).is_none());
        assert!(session_ref_from_report("custom:pi", Some("pi-id".into()), None).is_none());

        let session_ref = session_ref_from_report(
            "shepr:omp",
            Some("omp-id".into()),
            Some(omp_session.clone()),
        )
        .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Path);
        assert_eq!(session_ref.value_str(), omp_session);

        let session_ref = session_ref_from_report("shepr:omp", Some("omp-id".into()), None)
            .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "omp-id");
        let session_ref = session_ref_from_report(
            "shepr:omp",
            Some("omp-id".into()),
            Some("relative.jsonl".into()),
        )
        .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "omp-id");
        assert!(
            session_ref_from_report("shepr:omp", None, Some("relative.jsonl".into())).is_none()
        );

        assert!(session_ref_from_report("shepr:claude", None, Some(claude_session)).is_none());

        let session_ref = session_ref_from_report("shepr:copilot", Some("copilot-id".into()), None)
            .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "copilot-id");
        assert!(session_ref_from_report("shepr:copilot", None, Some(copilot_session)).is_none());

        let session_ref = session_ref_from_report("shepr:devin", Some("devin-id".into()), None)
            .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "devin-id");

        let session_ref = session_ref_from_report("shepr:droid", Some("droid-id".into()), None)
            .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "droid-id");
        assert!(
            session_ref_from_report("shepr:droid", None, Some("/tmp/droid-session".into()))
                .is_none()
        );

        let session_ref = session_ref_from_report("shepr:kimi", Some("kimi-id".into()), None)
            .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "kimi-id");

        let session_ref =
            session_ref_from_report("shepr:mastracode", Some("mastracode-id".into()), None)
                .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "mastracode-id");

        let session_ref = session_ref_from_report("shepr:kilo", Some("kilo-id".into()), None)
            .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "kilo-id");

        for source in ["shepr:qodercli", "shepr:qwen", "shepr:letta"] {
            assert!(session_ref_from_report(source, Some("id".into()), None).is_none());
        }

        let session_ref = session_ref_from_report("shepr:agy", Some("agy-id".into()), None)
            .expect("test precondition");
        assert_eq!(session_ref.kind(), AgentSessionRefKind::Id);
        assert_eq!(session_ref.value_str(), "agy-id");
    }

    #[test]
    fn session_start_source_parser_accepts_only_exact_known_values() {
        assert_eq!(
            AgentSessionStartSource::parse("startup"),
            Ok(AgentSessionStartSource::Startup)
        );
        assert_eq!(
            AgentSessionStartSource::parse("resume"),
            Ok(AgentSessionStartSource::Resume)
        );
        assert_eq!(
            AgentSessionStartSource::parse("clear"),
            Ok(AgentSessionStartSource::Clear)
        );
        assert_eq!(
            AgentSessionStartSource::parse("compact"),
            Ok(AgentSessionStartSource::Compact)
        );
        assert_eq!(
            AgentSessionStartSource::parse("new"),
            Ok(AgentSessionStartSource::New)
        );
        assert_eq!(
            AgentSessionStartSource::parse("load"),
            Ok(AgentSessionStartSource::Load)
        );
        assert_eq!(
            AgentSessionStartSource::parse("fork"),
            Ok(AgentSessionStartSource::Fork)
        );
        assert_eq!(
            AgentSessionStartSource::parse("select"),
            Ok(AgentSessionStartSource::Select)
        );
        assert_eq!(
            AgentSessionStartSource::parse(" resume ")
                .expect_err("source spelling is exact")
                .as_str(),
            " resume "
        );
        assert_eq!(
            AgentSessionStartSource::parse("other")
                .expect_err("unknown source is retained")
                .as_str(),
            "other"
        );
    }

    #[test]
    fn every_session_start_source_round_trips_through_its_string() {
        for source in AgentSessionStartSource::ALL {
            assert_eq!(AgentSessionStartSource::parse(source.as_str()), Ok(source));
        }
    }

    #[test]
    fn an_unrecognized_start_never_replaces_whatever_the_policy() {
        assert_eq!(
            ReportedSessionStart::from_wire(Some("future-source")),
            ReportedSessionStart::Unrecognized
        );
        assert_eq!(
            ReportedSessionStart::from_wire(None),
            ReportedSessionStart::Omitted
        );
        for descriptor in crate::AGENTS {
            assert!(
                !descriptor
                    .hook_session_policy()
                    .allows_replacement(ReportedSessionStart::Unrecognized),
                "{}",
                descriptor.label
            );
        }
        assert!(
            Agent::Antigravity
                .descriptor()
                .hook_session_policy()
                .allows_replacement(ReportedSessionStart::Omitted),
            "premise: some policy replaces on an omitted source"
        );
    }

    #[test]
    fn ids_are_data_not_shell_text() {
        let id = "abc; rm -rf /";
        let codex_plan = plan_for_source(
            "shepr:codex",
            &AgentSessionRef::id(id).expect("test precondition"),
        )
        .expect("test precondition");
        assert_eq!(codex_plan.argv, vec!["codex", "resume", id]);

        let copilot_plan = plan_for_source(
            "shepr:copilot",
            &AgentSessionRef::id(id).expect("test precondition"),
        )
        .expect("test precondition");
        assert_eq!(copilot_plan.argv, vec!["copilot", "--resume=abc; rm -rf /"]);

        let devin_plan = plan_for_source(
            "shepr:devin",
            &AgentSessionRef::id(id).expect("test precondition"),
        )
        .expect("test precondition");
        assert_eq!(devin_plan.argv, vec!["devin", "--resume", id]);
    }

    #[test]
    fn ids_that_read_as_flags_are_rejected_on_every_path() {
        for id in ["-", "--dangerously-skip-permissions", "-x"] {
            assert!(AgentSessionRef::id(id).is_none(), "{id}");
            assert!(session_ref_from_report("shepr:claude", Some(id.into()), None).is_none());
            assert!(
                snapshot_session_for_source("shepr:claude", AgentSessionRefKind::Id, id).is_none()
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
            plan_for_source(
                "shepr:opencode",
                &AgentSessionRef::path(&opencode_session).expect("test precondition")
            )
            .is_none()
        );
        assert!(
            plan_for_source(
                "shepr:kilo",
                &AgentSessionRef::path(&kilo_session).expect("test precondition")
            )
            .is_none()
        );
        assert!(
            plan_for_source(
                "shepr:copilot",
                &AgentSessionRef::path(&copilot_session).expect("test precondition")
            )
            .is_none()
        );
        assert!(
            plan_for_source(
                "shepr:devin",
                &AgentSessionRef::path(&devin_session).expect("test precondition")
            )
            .is_none()
        );
        assert!(
            snapshot_session_for_source(
                "shepr:mastracode",
                AgentSessionRefKind::Id,
                "mastracode-session"
            )
            .is_some()
        );
        assert!(
            snapshot_session_for_source(
                "shepr:opencode",
                AgentSessionRefKind::Id,
                "opencode-session"
            )
            .is_some()
        );
        assert!(
            snapshot_session_for_source("shepr:kilo", AgentSessionRefKind::Id, "kilo-session")
                .is_some()
        );
        assert!(
            snapshot_session_for_source(
                "shepr:copilot",
                AgentSessionRefKind::Id,
                "copilot-session"
            )
            .is_some()
        );
        assert!(
            snapshot_session_for_source("shepr:devin", AgentSessionRefKind::Id, "devin-session")
                .is_some()
        );
        assert!(
            snapshot_session_for_source("shepr:agy", AgentSessionRefKind::Id, "agy-session")
                .is_some()
        );
        let agy_session = absolute_test_path("agy-session");
        assert!(
            plan_for_source(
                "shepr:agy",
                &AgentSessionRef::path(&agy_session).expect("test precondition")
            )
            .is_none()
        );
    }
}
