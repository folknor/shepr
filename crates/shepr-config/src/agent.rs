/// Agent identity used by config validation and persisted preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ConfigAgent {
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
    Hermes,
    Kilo,
    Qodercli,
    Qwen,
    Letta,
    Maki,
    Muse,
}

impl ConfigAgent {
    pub const ALL: [Self; 24] = [
        Self::Pi,
        Self::Claude,
        Self::Codex,
        Self::Gemini,
        Self::Cursor,
        Self::Devin,
        Self::Antigravity,
        Self::Cline,
        Self::Omp,
        Self::Mastracode,
        Self::OpenCode,
        Self::GithubCopilot,
        Self::Kimi,
        Self::Kiro,
        Self::Droid,
        Self::Amp,
        Self::Grok,
        Self::Hermes,
        Self::Kilo,
        Self::Qodercli,
        Self::Qwen,
        Self::Letta,
        Self::Maki,
        Self::Muse,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor",
            Self::Devin => "devin",
            Self::Antigravity => "agy",
            Self::Cline => "cline",
            Self::Omp => "omp",
            Self::Mastracode => "mastracode",
            Self::OpenCode => "opencode",
            Self::GithubCopilot => "copilot",
            Self::Kimi => "kimi",
            Self::Kiro => "kiro",
            Self::Droid => "droid",
            Self::Amp => "amp",
            Self::Grok => "grok",
            Self::Hermes => "hermes",
            Self::Kilo => "kilo",
            Self::Qodercli => "qodercli",
            Self::Qwen => "qwen",
            Self::Letta => "letta",
            Self::Maki => "maki",
            Self::Muse => "muse",
        }
    }

    pub fn parse_canonical_label(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|agent| agent.label() == value)
    }

    pub fn parse_label(value: &str) -> Option<Self> {
        let name = value.trim().to_lowercase();
        let name = name
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or(&name);
        let name = name
            .strip_suffix(".exe")
            .or_else(|| name.strip_suffix(".js"))
            .unwrap_or(name);
        match name {
            "pi" => Some(Self::Pi),
            "claude" | "claude-code" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "gemini" => Some(Self::Gemini),
            "cursor" | "cursor-agent" => Some(Self::Cursor),
            "devin" | "devin-cli" | "devin cli" => Some(Self::Devin),
            "agy" | "antigravity" | "antigravity-cli" => Some(Self::Antigravity),
            "cline" | ".cline" => Some(Self::Cline),
            "omp" => Some(Self::Omp),
            "mastracode" | "mastra-code" | "mastra code" => Some(Self::Mastracode),
            "opencode" | "opencode2" | "open-code" => Some(Self::OpenCode),
            "copilot" | "github-copilot" | "ghcs" => Some(Self::GithubCopilot),
            "kimi" | "kimi-code" | "kimi code" => Some(Self::Kimi),
            "kiro" | "kiro-cli" => Some(Self::Kiro),
            "droid" => Some(Self::Droid),
            "amp" | "amp-local" => Some(Self::Amp),
            "grok" | "grok-build" => Some(Self::Grok),
            "hermes" | "hermes-agent" => Some(Self::Hermes),
            "kilo" | "kilo-code" | "kilo code" => Some(Self::Kilo),
            "qodercli" | "qoderclicn" | "qoder" | "qodercn" => Some(Self::Qodercli),
            "qwen" | "qwen-code" | "qwen code" => Some(Self::Qwen),
            "letta" | "letta-code" | "letta code" => Some(Self::Letta),
            "maki" => Some(Self::Maki),
            "muse" | "muse-code" | "muse-cli" => Some(Self::Muse),
            _ if name
                .strip_prefix("muse-bin-")
                .is_some_and(|version| version.starts_with(|ch: char| ch.is_ascii_digit())) =>
            {
                Some(Self::Muse)
            }
            _ => None,
        }
    }
}
