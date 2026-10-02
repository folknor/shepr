//! Canonical registrations shared by configuration edits and status checks.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use super::command::hook_command;
use super::config_edit::{
    ensure_command_hook, ensure_direct_command_hook, ensure_flat_command_hook,
    ensure_simple_command_hook,
};
use crate::agent::IntegrationTarget as Target;

#[derive(Clone, Copy)]
pub(super) enum HooksRoot {
    HooksKey,
    Document,
}

#[derive(Clone, Copy)]
pub(super) enum JsonShape {
    Nested(Duration),
    NestedClaude(Duration),
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
    },
    Codex {
        hooks: &'static str,
        config: &'static str,
        timeout: Duration,
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
    /// Event selection, action arguments, matcher, entry fields and timeout
    /// units are decided here. Install merges these entries and status matches
    /// them; neither reconstructs a second interpretation of the descriptor.
    pub(super) fn expected_events(
        self,
        target: Target,
        hook_path: &Path,
    ) -> io::Result<Map<String, Value>> {
        let mut entries = Map::new();
        for hook in target.hook_events() {
            // Copilot, Devin and Droid call their payload-decoding hooks for
            // every event. The other integrations require an explicit action.
            if hook.action.is_none()
                && !matches!(target, Target::Copilot | Target::Devin | Target::Droid)
            {
                continue;
            }
            let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
            let command = hook_command(hook_path, action);
            match self {
                JsonShape::Nested(timeout) => ensure_command_hook(
                    &mut entries,
                    hook.event,
                    &command,
                    timeout.as_secs(),
                    hook.matcher,
                )?,
                JsonShape::NestedClaude(timeout) => ensure_command_hook(
                    &mut entries,
                    hook.event,
                    &command,
                    timeout.as_secs(),
                    Some(&super::claude_settings::claude_session_start_matcher()),
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
                    hook.matcher,
                )?,
                JsonShape::Simple => {
                    ensure_simple_command_hook(&mut entries, hook.event, &command)?;
                }
            }
        }
        Ok(entries)
    }
}

pub(super) fn timeout_millis(timeout: Duration) -> io::Result<u64> {
    u64::try_from(timeout.as_millis())
        .map_err(|_| io::Error::other("hook timeout exceeds millisecond configuration range"))
}

#[cfg(test)]
impl Registration {
    pub(super) fn timeout(self) -> Option<Duration> {
        match self {
            Self::Json {
                shape:
                    JsonShape::Nested(timeout)
                    | JsonShape::NestedClaude(timeout)
                    | JsonShape::Flat(timeout)
                    | JsonShape::Direct(timeout),
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
