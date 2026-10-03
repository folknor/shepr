//! Agent identity and facts that must agree across detection, integrations,
//! resume and presentation.

mod report;
pub mod resume;

pub use report::{HookAuthorityClass, ReportOrigin, ReportOriginError, ReportedAgent};

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize, de::Visitor};

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
    Cursor,
    Mastracode,
    AntigravityCli,
    Grok,
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
pub(crate) const CLAUDE_SESSION_START_EVENT: IntegrationHookEvent =
    hook_event("SessionStart", None, Some(IntegrationHookAction::Session));
const CLAUDE_HOOK_EVENTS: &[IntegrationHookEvent] = &[CLAUDE_SESSION_START_EVENT];
const CODEX_HOOK_EVENTS: &[IntegrationHookEvent] = &[
    hook_event("SessionStart", None, Some(IntegrationHookAction::Session)),
    hook_event(
        "UserPromptSubmit",
        None,
        Some(IntegrationHookAction::Working),
    ),
    hook_event("Stop", None, Some(IntegrationHookAction::Idle)),
    hook_event("Interrupt", None, Some(IntegrationHookAction::Idle)),
];
const DEVIN_HOOK_EVENTS: &[IntegrationHookEvent] = &[
    hook_event("SessionStart", None, Some(IntegrationHookAction::Session)),
    hook_event(
        "UserPromptSubmit",
        None,
        Some(IntegrationHookAction::Session),
    ),
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
const CURSOR_HOOK_EVENTS: &[IntegrationHookEvent] = &[hook_event(
    "sessionStart",
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

/// Session transitions and report requirements of an integration's event vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookSessionPolicy {
    pub replacement_starts: &'static [resume::AgentSessionStartSource],
    pub replace_without_start: bool,
    /// Whether state events from this integration are invalid without a session reference.
    pub state_requires_session_ref: bool,
    pub state_requires_current_session: bool,
    pub unsequenced_selection: bool,
    pub foreground_takeover: bool,
}

impl HookSessionPolicy {
    const DEFAULT: Self = Self {
        replacement_starts: &[],
        replace_without_start: false,
        state_requires_session_ref: false,
        state_requires_current_session: false,
        unsequenced_selection: false,
        foreground_takeover: true,
    };
    // Claude's SessionStart sources are this list plus `startup`, and the
    // integration's hook matcher is built from it (`claude_settings`), so the
    // sources Claude reports and the sources that replace stay one list.
    // `startup` reports a new process, which has no live session in this pane
    // to replace. Every other source puts a different session id in the pane's
    // own process: `--fork-session` and `/branch` switch the pane into the
    // fork (Claude Code reported these as `resume` before it added `fork`).
    // The payload does not say which kind of fork it is: a `/fork` background
    // copy, which leaves the original in the pane, and a conversation moved to
    // the background also report `fork`. Those run as background sessions
    // under Claude's supervisor, whose environment is built from the
    // dispatching shell's and so can carry the pane's variables. Claude marks
    // every background session process with `CLAUDE_JOB_DIR` and
    // `CLAUDE_CODE_SESSION_KIND=bg`, and the hook asset reports nothing when
    // `CLAUDE_JOB_DIR` is set or the session kind is `bg` or one of the
    // supervisor's own, so a `fork` that reaches this policy came from the
    // pane's own process. Panes scrub both variables, so a server started
    // from inside a background session does not silence its panes' hooks.
    const CLAUDE: Self = Self {
        replacement_starts: &[
            resume::AgentSessionStartSource::Resume,
            resume::AgentSessionStartSource::Clear,
            resume::AgentSessionStartSource::Compact,
            resume::AgentSessionStartSource::Fork,
        ],
        ..Self::DEFAULT
    };
    const CODEX: Self = Self {
        replacement_starts: &[
            resume::AgentSessionStartSource::Startup,
            resume::AgentSessionStartSource::Clear,
            resume::AgentSessionStartSource::Resume,
            resume::AgentSessionStartSource::Compact,
        ],
        state_requires_session_ref: true,
        state_requires_current_session: true,
        ..Self::DEFAULT
    };
    const MASTRACODE: Self = Self {
        replacement_starts: &[resume::AgentSessionStartSource::Startup],
        state_requires_session_ref: true,
        ..Self::DEFAULT
    };
    const KILO: Self = Self {
        replacement_starts: &[resume::AgentSessionStartSource::Startup],
        state_requires_session_ref: true,
        ..Self::DEFAULT
    };
    const OPENCODE: Self = Self {
        replacement_starts: &[resume::AgentSessionStartSource::Select],
        unsequenced_selection: true,
        state_requires_session_ref: true,
        ..Self::DEFAULT
    };
    const PI: Self = Self {
        replacement_starts: &[
            resume::AgentSessionStartSource::New,
            resume::AgentSessionStartSource::Resume,
            resume::AgentSessionStartSource::Fork,
        ],
        state_requires_session_ref: true,
        ..Self::DEFAULT
    };
    const GROK: Self = Self {
        replacement_starts: &[
            resume::AgentSessionStartSource::New,
            resume::AgentSessionStartSource::Load,
        ],
        foreground_takeover: false,
        ..Self::DEFAULT
    };
    const OMP: Self = Self {
        replacement_starts: &[
            resume::AgentSessionStartSource::Startup,
            resume::AgentSessionStartSource::New,
            resume::AgentSessionStartSource::Resume,
            resume::AgentSessionStartSource::Fork,
        ],
        state_requires_session_ref: true,
        ..Self::DEFAULT
    };
    const ANTIGRAVITY: Self = Self {
        replace_without_start: true,
        ..Self::DEFAULT
    };
    // A recognized Kimi SessionStart is an explicit selection, never a state
    // report claiming a different session in the same live process.
    const KIMI: Self = Self {
        replacement_starts: &[
            resume::AgentSessionStartSource::Startup,
            resume::AgentSessionStartSource::New,
            resume::AgentSessionStartSource::Clear,
            resume::AgentSessionStartSource::Resume,
            resume::AgentSessionStartSource::Fork,
            resume::AgentSessionStartSource::Compact,
        ],
        state_requires_session_ref: true,
        ..Self::DEFAULT
    };
    pub fn allows_replacement(self, start: resume::ReportedSessionStart) -> bool {
        match start {
            resume::ReportedSessionStart::Omitted => self.replace_without_start,
            resume::ReportedSessionStart::Known(start) => self.replacement_starts.contains(&start),
            resume::ReportedSessionStart::Unrecognized => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentDescriptor {
    pub agent: Agent,
    pub label: &'static str,
    pub aliases: &'static [&'static str],
    pub executable: &'static str,
    pub integration: Option<IntegrationDescriptor>,
    pub resume_support: Option<ResumeSupport>,
    /// Bundled screen-detection rules, embedded with the owning agent descriptor.
    pub screen_manifest: Option<&'static str>,
}

pub const AGENTS: [AgentDescriptor; 23] = [
    AgentDescriptor {
        agent: Agent::Pi,
        label: "pi",
        aliases: &[],
        executable: "pi",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Pi,
            IntegrationCapability::FullLifecycle,
            HookSessionPolicy::PI,
            &[],
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::IdOrPath,
            ResumeArgs::FlagValue("--session"),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/pi.toml")),
    },
    AgentDescriptor {
        agent: Agent::Claude,
        label: "claude",
        aliases: &["claude-code"],
        executable: "claude",
        integration: Some(IntegrationDescriptor::claude()),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/claude.toml")),
    },
    AgentDescriptor {
        agent: Agent::Codex,
        label: "codex",
        aliases: &[],
        executable: "codex",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Codex,
            IntegrationCapability::PartialState,
            HookSessionPolicy::CODEX,
            CODEX_HOOK_EVENTS,
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::Subcommand("resume"),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/codex.toml")),
    },
    AgentDescriptor {
        agent: Agent::Gemini,
        label: "gemini",
        aliases: &[],
        executable: "gemini",
        integration: None,
        resume_support: None,
        screen_manifest: Some(include_str!("../detect/manifests/gemini.toml")),
    },
    AgentDescriptor {
        agent: Agent::Cursor,
        label: "cursor",
        aliases: &["cursor-agent"],
        executable: "cursor-agent",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Cursor,
            IntegrationCapability::ScreenOwnedSession,
            HookSessionPolicy::DEFAULT,
            CURSOR_HOOK_EVENTS,
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/cursor.toml")),
    },
    AgentDescriptor {
        agent: Agent::Devin,
        label: "devin",
        aliases: &["devin-cli", "devin cli"],
        executable: "devin",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Devin,
            IntegrationCapability::ScreenOwnedSession,
            HookSessionPolicy::DEFAULT,
            DEVIN_HOOK_EVENTS,
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/devin.toml")),
    },
    AgentDescriptor {
        agent: Agent::Antigravity,
        label: "agy",
        aliases: &["antigravity", "antigravity-cli"],
        executable: "agy",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::AntigravityCli,
            IntegrationCapability::IdentityOnly,
            HookSessionPolicy::ANTIGRAVITY,
            ANTIGRAVITY_HOOK_EVENTS,
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue(CONVERSATION_FLAG),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/antigravity.toml")),
    },
    AgentDescriptor {
        agent: Agent::Cline,
        label: "cline",
        aliases: &[".cline"],
        executable: "cline",
        integration: None,
        resume_support: None,
        screen_manifest: Some(include_str!("../detect/manifests/cline.toml")),
    },
    AgentDescriptor {
        agent: Agent::Omp,
        label: "omp",
        aliases: &[],
        executable: "omp",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Omp,
            IntegrationCapability::FullLifecycle,
            HookSessionPolicy::OMP,
            &[],
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::IdOrPath,
            ResumeArgs::InlineFlag("--resume="),
        )),
        screen_manifest: None,
    },
    AgentDescriptor {
        agent: Agent::Mastracode,
        label: "mastracode",
        aliases: &["mastra-code", "mastra code"],
        executable: "mastracode",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Mastracode,
            IntegrationCapability::FullLifecycle,
            HookSessionPolicy::MASTRACODE,
            MASTRACODE_HOOK_EVENTS,
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--thread"),
        )),
        screen_manifest: None,
    },
    AgentDescriptor {
        agent: Agent::OpenCode,
        label: "opencode",
        aliases: &["opencode2", "open-code"],
        executable: "opencode",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Opencode,
            IntegrationCapability::FullLifecycle,
            HookSessionPolicy::OPENCODE,
            &[],
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--session"),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/opencode.toml")),
    },
    AgentDescriptor {
        agent: Agent::GithubCopilot,
        label: "copilot",
        aliases: &["github-copilot", "ghcs"],
        executable: "copilot",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Copilot,
            IntegrationCapability::ScreenOwnedSession,
            HookSessionPolicy::DEFAULT,
            COPILOT_HOOK_EVENTS,
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::InlineFlag("--resume="),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/github-copilot.toml")),
    },
    AgentDescriptor {
        agent: Agent::Kimi,
        label: "kimi",
        aliases: &["kimi-code", "kimi code"],
        executable: "kimi",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Kimi,
            IntegrationCapability::FullLifecycle,
            HookSessionPolicy::KIMI,
            KIMI_HOOK_EVENTS,
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--session"),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/kimi.toml")),
    },
    AgentDescriptor {
        agent: Agent::Kiro,
        label: "kiro",
        aliases: &["kiro-cli"],
        executable: "kiro-cli",
        integration: None,
        resume_support: None,
        screen_manifest: Some(include_str!("../detect/manifests/kiro.toml")),
    },
    AgentDescriptor {
        agent: Agent::Droid,
        label: "droid",
        aliases: &[],
        executable: "droid",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Droid,
            IntegrationCapability::ScreenOwnedSession,
            HookSessionPolicy::DEFAULT,
            DROID_HOOK_EVENTS,
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/droid.toml")),
    },
    AgentDescriptor {
        agent: Agent::Amp,
        label: "amp",
        aliases: &["amp-local"],
        executable: "amp",
        integration: None,
        resume_support: None,
        screen_manifest: Some(include_str!("../detect/manifests/amp.toml")),
    },
    AgentDescriptor {
        agent: Agent::Grok,
        label: "grok",
        aliases: &["grok-build"],
        executable: "grok",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Grok,
            IntegrationCapability::ScreenOwnedSession,
            HookSessionPolicy::GROK,
            GROK_HOOK_EVENTS,
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--resume"),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/grok.toml")),
    },
    AgentDescriptor {
        agent: Agent::Kilo,
        label: "kilo",
        aliases: &["kilo-code", "kilo code"],
        executable: "kilo",
        integration: Some(IntegrationDescriptor::new(
            IntegrationTarget::Kilo,
            IntegrationCapability::FullLifecycle,
            HookSessionPolicy::KILO,
            &[],
        )),
        resume_support: Some(ResumeSupport::new(
            SessionRefPolicy::Id,
            ResumeArgs::FlagValue("--session"),
        )),
        screen_manifest: Some(include_str!("../detect/manifests/kilo.toml")),
    },
    AgentDescriptor {
        agent: Agent::Qodercli,
        label: "qodercli",
        aliases: &["qoderclicn", "qoder", "qodercn"],
        executable: "qodercli",
        integration: None,
        resume_support: None,
        screen_manifest: Some(include_str!("../detect/manifests/qodercli.toml")),
    },
    AgentDescriptor {
        agent: Agent::Qwen,
        label: "qwen",
        aliases: &["qwen-code", "qwen code"],
        executable: "qwen",
        integration: None,
        resume_support: None,
        screen_manifest: Some(include_str!("../detect/manifests/qwen.toml")),
    },
    AgentDescriptor {
        agent: Agent::Letta,
        label: "letta",
        aliases: &["letta-code", "letta code"],
        executable: "letta",
        integration: None,
        resume_support: None,
        screen_manifest: Some(include_str!("../detect/manifests/letta.toml")),
    },
    AgentDescriptor {
        agent: Agent::Maki,
        label: "maki",
        aliases: &[],
        executable: "maki",
        integration: None,
        resume_support: None,
        screen_manifest: Some(include_str!("../detect/manifests/maki.toml")),
    },
    AgentDescriptor {
        agent: Agent::Muse,
        label: "muse",
        aliases: &["muse-code", "muse-cli"],
        executable: "muse",
        integration: None,
        resume_support: None,
        screen_manifest: Some(include_str!("../detect/manifests/muse.toml")),
    },
];

// `Agent::descriptor` indexes this table by the enum discriminant. Keep a
// table reorder or a new variant from silently changing that lookup.
const _: () = {
    // Exhaustive on purpose: a new variant fails to compile here until it is
    // classified. The one arm returning true must be the enum's last declared
    // variant, and it must also end the table, so a variant with no
    // descriptor cannot index past it.
    const fn is_last_variant(agent: Agent) -> bool {
        match agent {
            Agent::Muse => true,
            Agent::Pi
            | Agent::Claude
            | Agent::Codex
            | Agent::Gemini
            | Agent::Cursor
            | Agent::Devin
            | Agent::Antigravity
            | Agent::Cline
            | Agent::Omp
            | Agent::Mastracode
            | Agent::OpenCode
            | Agent::GithubCopilot
            | Agent::Kimi
            | Agent::Kiro
            | Agent::Droid
            | Agent::Amp
            | Agent::Grok
            | Agent::Kilo
            | Agent::Qodercli
            | Agent::Qwen
            | Agent::Letta
            | Agent::Maki => false,
        }
    }
    // With the loop below, the last variant sits at the table's last index,
    // so the table covers every discriminant.
    assert!(is_last_variant(AGENTS[AGENTS.len() - 1].agent));
    let mut index = 0;
    while index < AGENTS.len() {
        assert!(AGENTS[index].agent as usize == index);
        index += 1;
    }
};

/// The four installed integration classes; absence is the fifth capability class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationCapability {
    ScreenOwnedSession,
    IdentityOnly,
    PartialState,
    FullLifecycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrationDescriptor {
    target: IntegrationTarget,
    pub capability: IntegrationCapability,
    session_policy: HookSessionPolicy,
    events: &'static [IntegrationHookEvent],
}

impl IntegrationDescriptor {
    const fn new(
        target: IntegrationTarget,
        capability: IntegrationCapability,
        session_policy: HookSessionPolicy,
        events: &'static [IntegrationHookEvent],
    ) -> Self {
        // Claude's source-preserving editor supports only its SessionStart group.
        assert!(!matches!(target, IntegrationTarget::Claude));
        Self {
            target,
            capability,
            session_policy,
            events,
        }
    }

    const fn claude() -> Self {
        Self {
            target: IntegrationTarget::Claude,
            capability: IntegrationCapability::ScreenOwnedSession,
            session_policy: HookSessionPolicy::CLAUDE,
            events: CLAUDE_HOOK_EVENTS,
        }
    }
}

impl AgentDescriptor {
    pub const fn hook_session_policy(&self) -> HookSessionPolicy {
        match self.integration {
            Some(integration) => integration.session_policy,
            None => HookSessionPolicy::DEFAULT,
        }
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
        match self.descriptor().integration {
            Some(integration) => Some(integration.target),
            None => None,
        }
    }

    pub const fn integration_source(self) -> Option<&'static str> {
        match self.integration_target() {
            Some(target) => Some(target.source()),
            None => None,
        }
    }

    pub const fn integration_hook_events(self) -> &'static [IntegrationHookEvent] {
        match self.descriptor().integration {
            Some(integration) => integration.events,
            None => &[],
        }
    }

    pub const fn screen_manifest(self) -> bool {
        self.descriptor().screen_manifest.is_some()
    }

    pub const fn screen_manifest_source(self) -> Option<&'static str> {
        self.descriptor().screen_manifest
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

    // This exhaustive inverse supplies const identity and policy access without
    // searching the descriptor table or introducing a missing-target fallback.
    // The descriptor audit checks the forward link against this inverse; file
    // formats, paths and installation strategies live in the integration spec.
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
            Self::Cursor => Agent::Cursor,
            Self::Mastracode => Agent::Mastracode,
            Self::AntigravityCli => Agent::Antigravity,
            Self::Grok => Agent::Grok,
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
    Official(IntegrationTarget),
    Custom(String),
}

impl AgentSource {
    pub fn parse(value: &str) -> Self {
        Agent::parse_source(value)
            .and_then(Agent::integration_target)
            .map_or_else(|| Self::Custom(value.to_owned()), Self::Official)
    }

    // Persisted string inputs and resume constructors still need an
    // exact official source/label pair. Live report arbitration uses the typed
    // ReportOrigin and does not use this narrower identity predicate.
    pub fn from_pair(source: &str, agent_label: &str) -> Option<Self> {
        let target = Agent::parse_canonical_label(agent_label)?.integration_target()?;
        (target.source() == source).then_some(Self::Official(target))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Official(target) => target.source(),
            Self::Custom(source) => source,
        }
    }

    pub const fn agent(&self) -> Option<Agent> {
        match self {
            Self::Official(target) => Some(target.agent()),
            Self::Custom(_) => None,
        }
    }
}

impl IntegrationTarget {
    pub const fn source(self) -> &'static str {
        match self {
            Self::Pi => "shepr:pi",
            Self::Omp => "shepr:omp",
            Self::Claude => "shepr:claude",
            Self::Codex => "shepr:codex",
            Self::Copilot => "shepr:copilot",
            Self::Devin => "shepr:devin",
            Self::Droid => "shepr:droid",
            Self::Kimi => "shepr:kimi",
            Self::Opencode => "shepr:opencode",
            Self::Kilo => "shepr:kilo",
            Self::Cursor => "shepr:cursor",
            Self::Mastracode => "shepr:mastracode",
            Self::AntigravityCli => "shepr:agy",
            Self::Grok => "shepr:grok",
        }
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
    fn official_sources_round_trip_with_nonempty_names() {
        for target in IntegrationTarget::all() {
            let source = AgentSource::Official(target);
            assert!(!source.as_str().is_empty());
            assert_eq!(target.agent().integration_source(), Some(target.source()));
            let json = serde_json::to_string(&source).expect("serialize source");
            assert_eq!(
                serde_json::from_str::<AgentSource>(&json).expect("deserialize source"),
                source
            );
        }
    }

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
            if let Some(target) = agent.integration_target() {
                assert_eq!(target.agent(), agent);
                assert_eq!(target.label(), descriptor.label);
                assert_eq!(target.hook_events(), agent.integration_hook_events());
                assert!(agent.integration_source().is_some());
            } else {
                assert!(agent.integration_hook_events().is_empty());
            }
            // Official sources identify installable integration targets.
            if let Some(source) = agent.integration_source() {
                assert_eq!(
                    AgentSource::from_pair(source, descriptor.label),
                    agent.integration_target().map(AgentSource::Official)
                );
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
    fn integration_classes_preserve_authority_for_every_agent() {
        use IntegrationCapability::{
            FullLifecycle, IdentityOnly, PartialState, ScreenOwnedSession,
        };
        let expected = [
            Some(FullLifecycle),
            Some(ScreenOwnedSession),
            Some(PartialState),
            None,
            Some(ScreenOwnedSession),
            Some(ScreenOwnedSession),
            Some(IdentityOnly),
            None,
            Some(FullLifecycle),
            Some(FullLifecycle),
            Some(FullLifecycle),
            Some(ScreenOwnedSession),
            Some(FullLifecycle),
            None,
            Some(ScreenOwnedSession),
            None,
            Some(ScreenOwnedSession),
            Some(FullLifecycle),
            None,
            None,
            None,
            None,
            None,
        ];
        for (descriptor, expected) in AGENTS.iter().zip(expected) {
            assert_eq!(
                descriptor
                    .integration
                    .map(|integration| integration.capability),
                expected
            );
            let authority = match expected {
                Some(ScreenOwnedSession | IdentityOnly) => HookAuthorityClass::SessionOnly,
                Some(FullLifecycle) => HookAuthorityClass::FullLifecycle,
                Some(PartialState) | None => HookAuthorityClass::PartialState,
            };
            assert_eq!(
                descriptor.hook_authority_class(),
                authority,
                "{}",
                descriptor.label
            );
        }
    }

    #[test]
    fn devin_registers_only_session_identity_events() {
        assert_eq!(
            IntegrationTarget::Devin
                .hook_events()
                .iter()
                .map(|event| event.event)
                .collect::<Vec<_>>(),
            ["SessionStart", "UserPromptSubmit"]
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
        assert!(crate::detect::TITLE_ACTIVITY_GLYPHS.contains('◐'));
        assert_eq!(
            Agent::screen_manifest_agents().count(),
            AGENTS
                .iter()
                .filter(|descriptor| descriptor.screen_manifest.is_some())
                .count()
        );
    }
}
