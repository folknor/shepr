//! Canonical registrations shared by configuration edits and status checks.

use crate::types::{InstallError, InstallResult};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use super::command::hook_command;
use super::config_edit::{
    ensure_command_hook, ensure_direct_command_hook, ensure_flat_command_hook,
    ensure_simple_command_hook,
};
use super::types::ArtifactRole;
use shepr_agent::Agent;
use shepr_agent::IntegrationTarget as Target;
use shepr_agent::resume::AgentSessionStartSource;

/// Claude's SessionStart sources: `startup`, which replaces nothing, then the
/// sources Claude's hook session policy treats as replacements. Deriving the
/// matcher from the policy keeps the reported and replacing sources one list;
/// the policy says why each source has its role.
pub(super) fn claude_session_start_sources() -> impl Iterator<Item = AgentSessionStartSource> {
    std::iter::once(AgentSessionStartSource::Startup).chain(
        Agent::Claude
            .descriptor()
            .hook_session_policy()
            .replacement_starts
            .iter()
            .copied(),
    )
}

pub(super) fn claude_session_start_matcher() -> String {
    let mut matcher = String::from("^(");
    for (index, source) in claude_session_start_sources().enumerate() {
        if index > 0 {
            matcher.push('|');
        }
        matcher.push_str(source.as_str());
    }
    matcher.push_str(")$");
    matcher
}

#[derive(Clone, Copy)]
pub(super) enum HooksRoot {
    HooksKey,
    // Root-level registration is currently MastraCode's format. Keep the
    // location generic; target wording belongs to its registration row.
    Document,
}

#[derive(Clone, Copy)]
pub(super) enum MatcherSource {
    Descriptor,
    ClaudeSessionStartPolicy,
}

#[derive(Clone, Copy)]
pub(super) struct HookEventPolicy {
    pub(super) matcher_source: MatcherSource,
    pub(super) decodes_events_without_action: bool,
}

impl HookEventPolicy {
    pub(super) const DESCRIPTOR: Self = Self {
        matcher_source: MatcherSource::Descriptor,
        decodes_events_without_action: false,
    };
    pub(super) const COPILOT: Self = Self {
        matcher_source: MatcherSource::Descriptor,
        decodes_events_without_action: true,
    };
    pub(super) const CLAUDE: Self = Self {
        matcher_source: MatcherSource::ClaudeSessionStartPolicy,
        decodes_events_without_action: false,
    };
}

#[derive(Clone, Copy)]
pub(super) struct RequiredJsonField {
    pub(super) key: &'static str,
    pub(super) default_number: u64,
}

impl RequiredJsonField {
    pub(super) fn value(self) -> Value {
        Value::from(self.default_number)
    }
}

#[derive(Clone, Copy)]
pub(super) enum JsonShape {
    Nested(Duration),
    Flat(Duration),
    Direct(Duration),
    Simple,
}

#[derive(Clone, Copy)]
pub(super) enum Registration {
    DirectoryLoaded,
    Json {
        file: &'static str,
        root: HooksRoot,
        shape: JsonShape,
        artifact_role: ArtifactRole,
        document_description: &'static str,
        required_fields: &'static [RequiredJsonField],
        event_policy: HookEventPolicy,
    },
    Codex {
        hooks: &'static str,
        config: &'static str,
        timeout: Duration,
        hooks_artifact_role: ArtifactRole,
        config_artifact_role: ArtifactRole,
        document_description: &'static str,
        required_fields: &'static [RequiredJsonField],
        event_policy: HookEventPolicy,
    },
    Kimi {
        file: &'static str,
        timeout: Duration,
    },
    AntigravityCli {
        file: &'static str,
        timeout: Duration,
    },
    Grok {
        file: &'static str,
        timeout: Duration,
    },
    Opencode,
}

impl Registration {
    pub(super) fn config_paths(self, dir: &Path) -> Vec<PathBuf> {
        let files = match self {
            Self::DirectoryLoaded => Vec::new(),
            Self::Json { file, .. }
            | Self::Kimi { file, .. }
            | Self::AntigravityCli { file, .. } => vec![file],
            Self::Grok { file, .. } => return vec![dir.join("hooks").join(file)],
            Self::Codex { hooks, config, .. } => vec![hooks, config],
            Self::Opencode => vec![
                super::OPENCODE_TUI_CONFIG_NAME,
                super::OPENCODE_LEGACY_TUI_CONFIG_NAME,
                super::OPENCODE_CLI_CONFIG_NAME,
            ],
        };
        files.into_iter().map(|file| dir.join(file)).collect()
    }
}

impl JsonShape {
    /// The registration row supplies event decoding and matcher policy. The
    /// shape decides entry fields and timeout units. Install and status use
    /// the same result instead of rebuilding separate interpretations.
    pub(super) fn expected_events(
        self,
        target: Target,
        hook_path: &Path,
        policy: HookEventPolicy,
    ) -> InstallResult<Map<String, Value>> {
        let mut entries = Map::new();
        let claude_matcher = match policy.matcher_source {
            MatcherSource::Descriptor => None,
            MatcherSource::ClaudeSessionStartPolicy => Some(claude_session_start_matcher()),
        };
        for hook in target.hook_events() {
            if hook.action.is_none() && !policy.decodes_events_without_action {
                continue;
            }
            let action = hook.action.map(shepr_agent::IntegrationHookAction::as_str);
            let command = hook_command(hook_path, action);
            let matcher = claude_matcher.as_deref().or(hook.matcher);
            match self {
                JsonShape::Nested(timeout) => ensure_command_hook(
                    &mut entries,
                    hook.event,
                    &command,
                    timeout.as_secs(),
                    matcher,
                )?,
                JsonShape::Flat(timeout) => ensure_flat_command_hook(
                    &mut entries,
                    hook.event,
                    &command,
                    timeout_millis(timeout)?,
                )?,
                JsonShape::Direct(timeout) => ensure_direct_command_hook(
                    &mut entries,
                    hook.event,
                    command,
                    timeout.as_secs(),
                    matcher,
                )?,
                JsonShape::Simple => {
                    ensure_simple_command_hook(&mut entries, hook.event, &command)?;
                }
            }
        }
        Ok(entries)
    }
}

pub(super) fn timeout_millis(timeout: Duration) -> InstallResult<u64> {
    u64::try_from(timeout.as_millis()).map_err(|_| {
        InstallError::from(io::Error::other(
            "hook timeout exceeds millisecond configuration range",
        ))
    })
}

#[cfg(test)]
impl Registration {
    pub(super) fn timeout(self) -> Option<Duration> {
        match self {
            Self::Json {
                shape:
                    JsonShape::Nested(timeout) | JsonShape::Flat(timeout) | JsonShape::Direct(timeout),
                ..
            }
            | Self::Codex { timeout, .. }
            | Self::Kimi { timeout, .. }
            | Self::AntigravityCli { timeout, .. }
            | Self::Grok { timeout, .. } => Some(timeout),
            _ => None,
        }
    }
}
