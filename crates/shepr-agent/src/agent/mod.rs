//! Agent identity and facts that must agree across detection, integrations,
//! resume, pane launch policy and presentation.

pub mod resume;

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize, de::Visitor};
use shepr_core::env::ChildEnv;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum Agent {
    Pi,
    Claude,
    Codex,
    Gemini,
    Cursor,
    Devin,
    Antigravity,
    Cline,
    Omp,
    Mastracode,
    OpenCode,
    GithubCopilot,
    Kimi,
    Kiro,
    Droid,
    Amp,
    Grok,
    Kilo,
    Qodercli,
    Qwen,
    Letta,
    Maki,
    Muse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationTarget {
    Pi,
    Omp,
    Claude,
    Codex,
    Copilot,
    Devin,
    Droid,
    Kimi,
    Opencode,
    Kilo,
    Qodercli,
    Qwen,
    Cursor,
    Mastracode,
    AntigravityCli,
    Grok,
    Letta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRefPolicy {
    Id,
    IdOrPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeArgs {
    FlagValue(&'static str),
    InlineFlag(&'static str),
    Subcommand(&'static str),
    LettaConversation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeSupport {
    pub session_ref_policy: SessionRefPolicy,
    pub resume_args: ResumeArgs,
}

impl ResumeSupport {
    pub const fn new(session_ref_policy: SessionRefPolicy, resume_args: ResumeArgs) -> Self {
        Self {
            session_ref_policy,
            resume_args,
        }
    }
}

const CONVERSATION_FLAG: &str = "--conversation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationHookAction {
    Session,
    Working,
    Blocked,
    Idle,
}

impl IntegrationHookAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrationHookEvent {
    pub event: &'static str,
    pub matcher: Option<&'static str>,
    pub action: Option<IntegrationHookAction>,
}

const fn hook_event(
    event: &'static str,
    matcher: Option<&'static str>,
    action: Option<IntegrationHookAction>,
) -> IntegrationHookEvent {
    IntegrationHookEvent {
        event,
        matcher,
        action,
    }
}

pub(crate) const KIMI_OTHER_TOOL_MATCHER: &str = "^(?!AskUserQuestion$).*$";
pub(crate) const KIMI_ASK_USER_QUESTION_MATCHER: &str = "^AskUserQuestion$";
const KIMI_HOOK_EVENTS: &[IntegrationHookEvent] = &[
    hook_event("SessionStart", None, Some(IntegrationHookAction::Session)),
    hook_event(
        "UserPromptSubmit",
        None,
        Some(IntegrationHookAction::Working),
    ),
    hook_event(
        "PreToolUse",
        Some(KIMI_OTHER_TOOL_MATCHER),
        Some(IntegrationHookAction::Working),
    ),
    hook_event(
        "PreToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        Some(IntegrationHookAction::Blocked),
    ),
    hook_event(
        "PostToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        Some(IntegrationHookAction::Working),
    ),
    hook_event(
        "PostToolUseFailure",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        Some(IntegrationHookAction::Working),
    ),
    hook_event("SubagentStart", None, Some(IntegrationHookAction::Working)),
    hook_event("PreCompact", None, Some(IntegrationHookAction::Working)),
    hook_event(
        "PermissionRequest",
        None,
        Some(IntegrationHookAction::Blocked),
    ),
    hook_event(
        "PermissionResult",
        None,
        Some(IntegrationHookAction::Working),
    ),
    hook_event("Stop", None, Some(IntegrationHookAction::Idle)),
    hook_event("Interrupt", None, Some(IntegrationHookAction::Idle)),
];
const COPILOT_HOOK_EVENTS: &[IntegrationHookEvent] = &[hook_event("SessionStart", None, None)];
const CLAUDE_HOOK_EVENTS: &[IntegrationHookEvent] = &[hook_event(
    "SessionStart",
    None,
    Some(IntegrationHookAction::Session),
)];
const CODEX_HOOK_EVENTS: &[IntegrationHookEvent] = &[hook_event(
    "SessionStart",
    None,
    Some(IntegrationHookAction::Session),
)];
const DEVIN_HOOK_EVENTS: &[IntegrationHookEvent] = &[
    hook_event("SessionStart", None, Some(IntegrationHookAction::Session)),
    hook_event(
        "UserPromptSubmit",
        None,
        Some(IntegrationHookAction::Session),
    ),
    hook_event("PreToolUse", None, Some(IntegrationHookAction::Session)),
    hook_event("PostToolUse", None, Some(IntegrationHookAction::Session)),
    hook_event(
        "PermissionRequest",
        None,
        Some(IntegrationHookAction::Session),
    ),
    hook_event("Stop", None, Some(IntegrationHookAction::Session)),
];
const DROID_HOOK_EVENTS: &[IntegrationHookEvent] = &[hook_event(
    "SessionStart",
    None,
    Some(IntegrationHookAction::Session),
)];
const GROK_HOOK_EVENTS: &[IntegrationHookEvent] = &[hook_event(
    "SessionStart",
    None,
    Some(IntegrationHookAction::Session),
)];
const LETTA_HOOK_EVENTS: &[IntegrationHookEvent] = &[hook_event(
    "SessionStart",
    None,
    Some(IntegrationHookAction::Session),
)];
const CURSOR_HOOK_EVENTS: &[IntegrationHookEvent] = &[hook_event(
    "sessionStart",
    None,
    Some(IntegrationHookAction::Session),
)];
const QODERCLI_HOOK_EVENTS: &[IntegrationHookEvent] = &[hook_event(
    "SessionStart",
    None,
    Some(IntegrationHookAction::Session),
)];
const QWEN_HOOK_EVENTS: &[IntegrationHookEvent] = &[hook_event(
    "SessionStart",
    None,
    Some(IntegrationHookAction::Session),
)];
const ANTIGRAVITY_HOOK_EVENTS: &[IntegrationHookEvent] = &[
    // PreInvocation supplies conversationId. Other events cannot report a safe
    // lifecycle state, and Stop denotes end of turn rather than process exit.
    hook_event("PreInvocation", None, Some(IntegrationHookAction::Session)),
];
const MASTRACODE_HOOK_EVENTS: &[IntegrationHookEvent] = &[
    hook_event("SessionStart", None, Some(IntegrationHookAction::Session)),
    hook_event(
        "UserPromptSubmit",
        None,
        Some(IntegrationHookAction::Working),
    ),
    hook_event("AgentStart", None, Some(IntegrationHookAction::Working)),
    hook_event("PreToolUse", None, Some(IntegrationHookAction::Working)),
    hook_event(
        "PermissionRequest",
        None,
        Some(IntegrationHookAction::Blocked),
    ),
    hook_event(
        "PermissionResult",
        None,
        Some(IntegrationHookAction::Working),
    ),
    hook_event("SubagentStart", None, Some(IntegrationHookAction::Working)),
    hook_event("SubagentEnd", None, Some(IntegrationHookAction::Working)),
    hook_event("Interrupt", None, Some(IntegrationHookAction::Idle)),
    hook_event("AgentEnd", None, Some(IntegrationHookAction::Idle)),
    hook_event("Stop", None, Some(IntegrationHookAction::Idle)),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentDescriptor {
    pub agent: Agent,
    pub label: &'static str,
    pub aliases: &'static [&'static str],
    pub executable: &'static str,
    pub integration_target: Option<IntegrationTarget>,
    pub integration_source: Option<&'static str>,
    pub reserves_native_state: bool,
    pub full_lifecycle_hook_authority: bool,
    pub session_identity_only_integration: bool,
    pub resume_support: Option<ResumeSupport>,
    pub screen_manifest: bool,
    pub env_to_scrub: &'static [ChildEnv],
    pub title_activity_glyphs: &'static str,
    pub prompt_observation: bool,
    pub integration_hook_events: &'static [IntegrationHookEvent],
}

const CLAUDE_ACTIVITY_GLYPHS: &str = "·\u{2722}\u{2733}\u{2736}\u{273B}\u{273D}◐◓◑◒";

pub const AGENTS: [AgentDescriptor; 23] = [
    AgentDescriptor {
        agent: Agent::Pi,
        label: "pi",
        aliases: &[],
        executable: "pi",
        integration_target: Some(IntegrationTarget::Pi),
        integration_source: Some("shepr:pi"),
        reserves_native_state: false,
        full_lifecycle_hook_authority: true,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::IdOrPath,
            ResumeArgs::FlagValue("--session"),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    },
    AgentDescriptor {
        agent: Agent::Claude,
        label: "claude",
        aliases: &["claude-code"],
        executable: "claude",
        integration_target: Some(IntegrationTarget::Claude),
        integration_source: Some("shepr:claude"),
        reserves_native_state: true,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: true,
        env_to_scrub: &[
            ChildEnv::ClaudeCode,
            ChildEnv::ClaudeCodeChildSession,
            ChildEnv::ClaudeCodeSessionId,
            ChildEnv::ClaudeCodeMessagingToken,
        ],
        title_activity_glyphs: CLAUDE_ACTIVITY_GLYPHS,
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(CLAUDE_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Codex,
        label: "codex",
        aliases: &[],
        executable: "codex",
        integration_target: Some(IntegrationTarget::Codex),
        integration_source: Some("shepr:codex"),
        reserves_native_state: true,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::Subcommand("resume"),
        )),
        screen_manifest: true,
        env_to_scrub: &[ChildEnv::CodexThreadId],
        title_activity_glyphs: "",
        prompt_observation: true,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(CODEX_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Gemini,
        label: "gemini",
        aliases: &[],
        executable: "gemini",
        integration_target: None,
        integration_source: None,
        reserves_native_state: false,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: None,
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    },
    AgentDescriptor {
        agent: Agent::Cursor,
        label: "cursor",
        aliases: &["cursor-agent"],
        executable: "cursor-agent",
        integration_target: Some(IntegrationTarget::Cursor),
        integration_source: Some("shepr:cursor"),
        reserves_native_state: true,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(CURSOR_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Devin,
        label: "devin",
        aliases: &["devin-cli", "devin cli"],
        executable: "devin",
        integration_target: Some(IntegrationTarget::Devin),
        integration_source: Some("shepr:devin"),
        reserves_native_state: true,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(DEVIN_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Antigravity,
        label: "agy",
        aliases: &["antigravity", "antigravity-cli"],
        executable: "agy",
        integration_target: Some(IntegrationTarget::AntigravityCli),
        integration_source: Some("shepr:agy"),
        reserves_native_state: false,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: true,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue(CONVERSATION_FLAG),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(ANTIGRAVITY_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Cline,
        label: "cline",
        aliases: &[".cline"],
        executable: "cline",
        integration_target: None,
        integration_source: None,
        reserves_native_state: false,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: None,
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    },
    AgentDescriptor {
        agent: Agent::Omp,
        label: "omp",
        aliases: &[],
        executable: "omp",
        integration_target: Some(IntegrationTarget::Omp),
        integration_source: Some("shepr:omp"),
        reserves_native_state: false,
        full_lifecycle_hook_authority: true,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::IdOrPath,
            ResumeArgs::InlineFlag("--resume="),
        )),
        screen_manifest: false,
        env_to_scrub: &[ChildEnv::Ompcode],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    },
    AgentDescriptor {
        agent: Agent::Mastracode,
        label: "mastracode",
        aliases: &["mastra-code", "mastra code"],
        executable: "mastracode",
        integration_target: Some(IntegrationTarget::Mastracode),
        integration_source: Some("shepr:mastracode"),
        reserves_native_state: false,
        full_lifecycle_hook_authority: true,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--thread"),
        )),
        screen_manifest: false,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(MASTRACODE_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::OpenCode,
        label: "opencode",
        aliases: &["opencode2", "open-code"],
        executable: "opencode",
        integration_target: Some(IntegrationTarget::Opencode),
        integration_source: Some("shepr:opencode"),
        reserves_native_state: false,
        full_lifecycle_hook_authority: true,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--session"),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    },
    AgentDescriptor {
        agent: Agent::GithubCopilot,
        label: "copilot",
        aliases: &["github-copilot", "ghcs"],
        executable: "copilot",
        integration_target: Some(IntegrationTarget::Copilot),
        integration_source: Some("shepr:copilot"),
        reserves_native_state: true,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::InlineFlag("--resume="),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(COPILOT_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Kimi,
        label: "kimi",
        aliases: &["kimi-code", "kimi code"],
        executable: "kimi",
        integration_target: Some(IntegrationTarget::Kimi),
        integration_source: Some("shepr:kimi"),
        reserves_native_state: false,
        full_lifecycle_hook_authority: true,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--session"),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(KIMI_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Kiro,
        label: "kiro",
        aliases: &["kiro-cli"],
        executable: "kiro-cli",
        integration_target: None,
        integration_source: None,
        reserves_native_state: false,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: None,
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    },
    AgentDescriptor {
        agent: Agent::Droid,
        label: "droid",
        aliases: &[],
        executable: "droid",
        integration_target: Some(IntegrationTarget::Droid),
        integration_source: Some("shepr:droid"),
        reserves_native_state: true,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(DROID_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Amp,
        label: "amp",
        aliases: &["amp-local"],
        executable: "amp",
        integration_target: None,
        integration_source: None,
        reserves_native_state: false,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: None,
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    },
    AgentDescriptor {
        agent: Agent::Grok,
        label: "grok",
        aliases: &["grok-build"],
        executable: "grok",
        integration_target: Some(IntegrationTarget::Grok),
        integration_source: Some("shepr:grok"),
        reserves_native_state: true,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(GROK_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Kilo,
        label: "kilo",
        aliases: &["kilo-code", "kilo code"],
        executable: "kilo",
        integration_target: Some(IntegrationTarget::Kilo),
        integration_source: Some("shepr:kilo"),
        reserves_native_state: false,
        full_lifecycle_hook_authority: true,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--session"),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    },
    AgentDescriptor {
        agent: Agent::Qodercli,
        label: "qodercli",
        aliases: &["qoderclicn", "qoder", "qodercn"],
        executable: "qodercli",
        integration_target: Some(IntegrationTarget::Qodercli),
        integration_source: Some("shepr:qodercli"),
        reserves_native_state: true,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(QODERCLI_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Qwen,
        label: "qwen",
        aliases: &["qwen-code", "qwen code"],
        executable: "qwen",
        integration_target: Some(IntegrationTarget::Qwen),
        integration_source: Some("shepr:qwen"),
        reserves_native_state: true,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: true,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(QWEN_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Letta,
        label: "letta",
        aliases: &["letta-code", "letta code"],
        executable: "letta",
        integration_target: Some(IntegrationTarget::Letta),
        integration_source: Some("shepr:letta"),
        reserves_native_state: false,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: true,
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::LettaConversation,
        )),
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    }
    .with_integration_hook_events(LETTA_HOOK_EVENTS),
    AgentDescriptor {
        agent: Agent::Maki,
        label: "maki",
        aliases: &[],
        executable: "maki",
        integration_target: None,
        integration_source: None,
        reserves_native_state: false,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: None,
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    },
    AgentDescriptor {
        agent: Agent::Muse,
        label: "muse",
        aliases: &["muse-code", "muse-cli"],
        executable: "muse",
        integration_target: None,
        integration_source: None,
        reserves_native_state: false,
        full_lifecycle_hook_authority: false,
        session_identity_only_integration: false,
        resume_support: None,
        screen_manifest: true,
        env_to_scrub: &[],
        title_activity_glyphs: "",
        prompt_observation: false,
        integration_hook_events: &[],
    },
];

impl AgentDescriptor {
    const fn with_integration_hook_events(
        mut self,
        events: &'static [IntegrationHookEvent],
    ) -> Self {
        self.integration_hook_events = events;
        self
    }
}

impl Agent {
    pub fn all() -> impl ExactSizeIterator<Item = Self> {
        AGENTS.iter().map(|descriptor| descriptor.agent)
    }

    pub const fn descriptor(self) -> &'static AgentDescriptor {
        &AGENTS[self as usize]
    }

    pub const fn label(self) -> &'static str {
        self.descriptor().label
    }

    pub const fn executable(self) -> &'static str {
        self.descriptor().executable
    }

    pub const fn integration_target(self) -> Option<IntegrationTarget> {
        self.descriptor().integration_target
    }

    pub const fn integration_source(self) -> Option<&'static str> {
        self.descriptor().integration_source
    }

    pub const fn integration_hook_events(self) -> &'static [IntegrationHookEvent] {
        self.descriptor().integration_hook_events
    }

    pub const fn screen_manifest(self) -> bool {
        self.descriptor().screen_manifest
    }

    pub const fn env_to_scrub(self) -> &'static [ChildEnv] {
        self.descriptor().env_to_scrub
    }

    pub const fn activity_glyphs(self) -> &'static str {
        self.descriptor().title_activity_glyphs
    }

    /// Whether this agent's OSC title can start with a recognized activity glyph.
    pub fn has_title_activity_glyph(self, glyph: char) -> bool {
        is_braille_activity_glyph(glyph) || self.activity_glyphs().contains(glyph)
    }

    pub const fn prompt_observation(self) -> bool {
        self.descriptor().prompt_observation
    }

    // Prompt readiness is an auxiliary boolean alongside Codex state detection.
    // Manifest rules select an AgentState, so expressing this as an idle rule
    // would alter state selection rather than report the same readiness signal.
    pub fn prompt_ready(self, content: &str) -> bool {
        if !self.descriptor().prompt_observation {
            return false;
        }
        contains_recent_non_whitespace(content, "›AskCodextodoanything")
            && !contains_recent_non_whitespace(content, "model:loading")
            && !contains_recent_non_whitespace(content, "Resumingsession")
    }

    pub fn parse_label(value: &str) -> Option<Self> {
        // `detect::parse_agent_label` normalizes case and executable suffixes
        // before reaching this lookup. Avoid another allocation on the /proc
        // process probe path.
        let value = value.trim();
        let value = value
            .rsplit('/')
            .find(|component| !component.is_empty())
            .unwrap_or(value);
        agent_name_lookup()
            .get(value)
            .copied()
            .or_else(|| muse_versioned_binary(value).then_some(Self::Muse))
    }

    pub fn parse_canonical_label(value: &str) -> Option<Self> {
        Self::all().find(|agent| agent.label() == value)
    }

    pub fn parse_source(value: &str) -> Option<Self> {
        Self::all().find(|agent| agent.integration_source() == Some(value))
    }

    pub fn screen_manifest_agents() -> impl Iterator<Item = Self> {
        Self::all().filter(|agent| agent.screen_manifest())
    }
}

const BRAILLE_ACTIVITY_GLYPH_RANGE: std::ops::RangeInclusive<char> = '\u{2800}'..='\u{28ff}';

fn is_braille_activity_glyph(glyph: char) -> bool {
    BRAILLE_ACTIVITY_GLYPH_RANGE.contains(&glyph)
}

fn contains_recent_non_whitespace(content: &str, needle: &str) -> bool {
    const MAX_PROMPT_READY_NEEDLE_CHARS: usize = 32;

    let mut needle_chars = ['\0'; MAX_PROMPT_READY_NEEDLE_CHARS];
    let mut needle_len = 0;
    for character in needle.chars().rev() {
        let Some(slot) = needle_chars.get_mut(needle_len) else {
            return false;
        };
        *slot = character;
        needle_len += 1;
    }
    if needle_len == 0 {
        return true;
    }

    let needle = &needle_chars[..needle_len];
    let mut prefix = [0; MAX_PROMPT_READY_NEEDLE_CHARS];
    for index in 1..needle_len {
        let mut matched = prefix[index - 1];
        while matched > 0 && needle[index] != needle[matched] {
            matched = prefix[matched - 1];
        }
        if needle[index] == needle[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }

    // Reverse both streams so the last twelve lines can be searched without buffering.
    let recent_lines = content.lines().rev().take(12);
    let mut matched = 0;
    for character in recent_lines.flat_map(|line| line.chars().rev()) {
        if character.is_whitespace() {
            continue;
        }
        while matched > 0 && character != needle[matched] {
            matched = prefix[matched - 1];
        }
        if character == needle[matched] {
            matched += 1;
            if matched == needle_len {
                return true;
            }
        }
    }
    false
}

pub fn launch_env_to_scrub() -> impl Iterator<Item = &'static str> {
    Agent::all().flat_map(|agent| agent.env_to_scrub().iter().copied().map(ChildEnv::name))
}

fn agent_name_lookup() -> &'static HashMap<&'static str, Agent> {
    static LOOKUP: OnceLock<HashMap<&'static str, Agent>> = OnceLock::new();
    LOOKUP.get_or_init(|| {
        let mut names = HashMap::new();
        for agent in Agent::all() {
            let descriptor = agent.descriptor();
            names.insert(descriptor.label, agent);
            for alias in descriptor.aliases {
                names.insert(*alias, agent);
            }
        }
        names
    })
}

fn muse_versioned_binary(value: &str) -> bool {
    value
        .strip_prefix("muse-bin-")
        .is_some_and(|version| version.starts_with(|c: char| c.is_ascii_digit()))
}

impl IntegrationTarget {
    pub fn all() -> impl Iterator<Item = Self> {
        Agent::all().filter_map(Agent::integration_target)
    }

    pub const fn agent(self) -> Agent {
        match self {
            Self::Pi => Agent::Pi,
            Self::Omp => Agent::Omp,
            Self::Claude => Agent::Claude,
            Self::Codex => Agent::Codex,
            Self::Copilot => Agent::GithubCopilot,
            Self::Devin => Agent::Devin,
            Self::Droid => Agent::Droid,
            Self::Kimi => Agent::Kimi,
            Self::Opencode => Agent::OpenCode,
            Self::Kilo => Agent::Kilo,
            Self::Qodercli => Agent::Qodercli,
            Self::Qwen => Agent::Qwen,
            Self::Cursor => Agent::Cursor,
            Self::Mastracode => Agent::Mastracode,
            Self::AntigravityCli => Agent::Antigravity,
            Self::Grok => Agent::Grok,
            Self::Letta => Agent::Letta,
        }
    }

    pub const fn label(self) -> &'static str {
        self.agent().label()
    }

    pub const fn hook_events(self) -> &'static [IntegrationHookEvent] {
        self.agent().integration_hook_events()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentSource {
    Official(Agent),
    Custom(String),
}

impl AgentSource {
    pub fn official(agent: Agent) -> Option<Self> {
        agent.integration_source().map(|_| Self::Official(agent))
    }

    pub fn parse(value: &str) -> Self {
        Agent::parse_source(value).map_or_else(|| Self::Custom(value.to_owned()), Self::Official)
    }

    pub fn from_pair(source: &str, agent_label: &str) -> Option<Self> {
        let agent = Agent::parse_canonical_label(agent_label)?;
        (agent.integration_source() == Some(source)).then_some(Self::Official(agent))
    }

    /// Returns an owned projection for state records that take ownership of the source.
    pub fn to_source_string(&self) -> String {
        self.as_str().to_owned()
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Official(agent) => agent.integration_source().unwrap_or_default(),
            Self::Custom(source) => source,
        }
    }

    pub const fn agent(&self) -> Option<Agent> {
        match self {
            Self::Official(agent) => Some(*agent),
            Self::Custom(_) => None,
        }
    }
}

impl From<String> for AgentSource {
    fn from(value: String) -> Self {
        Self::parse(&value)
    }
}

impl From<&str> for AgentSource {
    fn from(value: &str) -> Self {
        Self::parse(value)
    }
}

// Formatting and string comparisons let callers use the borrowed projection
// without allocating the owned value needed by state records.
impl PartialEq<&str> for AgentSource {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<&str> for Agent {
    fn eq(&self, other: &&str) -> bool {
        self.label() == *other
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl fmt::Display for AgentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Agent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.label())
    }
}

impl<'de> Deserialize<'de> for Agent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct AgentVisitor;
        impl Visitor<'_> for AgentVisitor {
            type Value = Agent;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical agent label")
            }
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Agent::parse_canonical_label(value)
                    .ok_or_else(|| E::custom(format!("unknown agent label: {value}")))
            }
        }
        deserializer.deserialize_str(AgentVisitor)
    }
}

impl Serialize for AgentSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SourceVisitor;
        impl Visitor<'_> for SourceVisitor {
            type Value = AgentSource;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an integration source string")
            }
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(AgentSource::parse(value))
            }
        }
        deserializer.deserialize_str(SourceVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_are_the_domain_source_for_agent_views() {
        let agents = Agent::all().collect::<Vec<_>>();
        assert_eq!(AGENTS.len(), agents.len());
        for (index, descriptor) in AGENTS.iter().enumerate() {
            let agent = agents[index];
            assert_eq!(descriptor.agent, agent);
            assert_eq!(agent.descriptor(), descriptor);
            assert_eq!(Agent::parse_canonical_label(descriptor.label), Some(agent));
            assert_eq!(Agent::parse_label(descriptor.label), Some(agent));
            if let Some(target) = descriptor.integration_target {
                assert_eq!(target.agent(), agent);
                assert_eq!(target.label(), descriptor.label);
                assert_eq!(target.hook_events(), agent.integration_hook_events());
                assert_eq!(
                    AgentSource::from_pair(
                        descriptor.integration_source.expect("integration source"),
                        descriptor.label,
                    ),
                    Some(AgentSource::Official(agent))
                );
            } else {
                assert!(descriptor.integration_source.is_none());
            }
        }
        assert_eq!(
            IntegrationTarget::all().count(),
            Agent::all()
                .filter(|agent| agent.integration_target().is_some())
                .count()
        );
    }

    #[test]
    fn agent_specific_policies_follow_the_descriptor() {
        assert_eq!(
            Agent::Antigravity.integration_target(),
            Some(IntegrationTarget::AntigravityCli)
        );
        assert_eq!(Agent::Antigravity.label(), "agy");
        assert_eq!(Agent::Antigravity.integration_source(), Some("shepr:agy"));
        assert!(Agent::Claude.activity_glyphs().contains('◐'));
        assert!(Agent::Claude.env_to_scrub().contains(&ChildEnv::ClaudeCode));
        assert!(
            Agent::Codex
                .env_to_scrub()
                .contains(&ChildEnv::CodexThreadId)
        );
        assert!(Agent::Omp.env_to_scrub().contains(&ChildEnv::Ompcode));
        assert!(Agent::Codex.prompt_ready("› Ask Codex to do anything"));
        assert!(Agent::Codex.prompt_ready("› Ask Codex to do\nanything"));
        assert!(!Agent::Codex.prompt_ready("› Ask Codex to do anything\nmodel:\nloading"));
        assert!(!Agent::Claude.prompt_ready("› Ask Codex to do anything"));
        assert_eq!(
            Agent::screen_manifest_agents().count(),
            AGENTS
                .iter()
                .filter(|descriptor| descriptor.screen_manifest)
                .count()
        );
    }
}
