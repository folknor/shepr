use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::IntegrationTarget as Target;

use super::command::hook_command;
use super::config_edit::{direct_command_field, is_matching_command_hook};
use super::env::{AgentIntegrationPaths, DirectoryKey};
use super::types::{InstallOutcome, UninstallOutcome};

pub fn integration_target_label(target: crate::agent::IntegrationTarget) -> &'static str {
    target.label()
}

#[derive(Clone, Copy)]
struct IntegrationSpec {
    target: Target,
    assets: &'static [&'static str],
    directory: DirectoryKey,
    /// Agent-owned config files in `directory` that install edits. Install
    /// and uninstall vet them before touching anything, and the registration
    /// check reads them from the directory `path` is installed under.
    config_files: &'static [&'static str],
    /// How status confirms the agent's own config still runs the hook.
    registration: RegistrationCheck,
    path: &'static [&'static str],
    version: u32,
    hook_timeout: Option<Duration>,
    events: &'static [crate::agent::IntegrationHookEvent],
    action_label: &'static str,
    install: fn(&AgentIntegrationPaths) -> io::Result<InstallOutcome>,
    uninstall: fn(&AgentIntegrationPaths) -> io::Result<UninstallOutcome>,
}

#[derive(Clone, Copy)]
enum RegistrationCheck {
    DirectoryLoaded,
    Json { root: HooksRoot, shape: JsonShape },
    Codex,
    Kimi,
    AntigravityCli,
    Grok,
    Opencode,
}

#[derive(Clone, Copy)]
enum JsonShape {
    Nested,
    NestedStar,
    NestedClaude,
    Flat,
    Direct,
    Simple,
}

const INTEGRATION_SPECS: &[IntegrationSpec] = &[
    IntegrationSpec {
        target: Target::Pi,
        config_files: &[],
        registration: RegistrationCheck::DirectoryLoaded,
        action_label: "pi",
        install: super::targets::install_pi,
        uninstall: super::targets::uninstall_pi,
        assets: &[super::PI_EXTENSION_ASSET],
        directory: DirectoryKey::PiExtension,
        path: &[super::PI_EXTENSION_INSTALL_NAME],
        version: super::PI_INTEGRATION_VERSION,
        hook_timeout: None,
        events: Target::Pi.hook_events(),
    },
    IntegrationSpec {
        target: Target::Omp,
        config_files: &[],
        registration: RegistrationCheck::DirectoryLoaded,
        action_label: "omp",
        install: super::targets::install_omp,
        uninstall: super::targets::uninstall_omp,
        assets: &[super::OMP_EXTENSION_ASSET],
        directory: DirectoryKey::OmpExtension,
        path: &[super::OMP_EXTENSION_INSTALL_NAME],
        version: super::OMP_INTEGRATION_VERSION,
        hook_timeout: None,
        events: Target::Omp.hook_events(),
    },
    IntegrationSpec {
        target: Target::Claude,
        config_files: &[super::CLAUDE_SETTINGS_NAME],
        registration: RegistrationCheck::Json {
            root: HooksRoot::HooksKey,
            shape: JsonShape::NestedClaude,
        },
        action_label: "claude",
        install: super::targets::install_claude,
        uninstall: super::targets::uninstall_claude,
        assets: &[super::CLAUDE_HOOK_ASSET],
        directory: DirectoryKey::Claude,
        path: &["hooks", super::CLAUDE_HOOK_INSTALL_NAME],
        version: super::CLAUDE_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Claude.hook_events(),
    },
    IntegrationSpec {
        target: Target::Codex,
        config_files: &[super::CODEX_HOOKS_NAME, super::CODEX_CONFIG_NAME],
        registration: RegistrationCheck::Codex,
        action_label: "codex",
        install: super::targets::install_codex,
        uninstall: super::targets::uninstall_codex,
        assets: &[super::CODEX_HOOK_ASSET],
        directory: DirectoryKey::Codex,
        path: &[super::CODEX_HOOK_INSTALL_NAME],
        version: super::CODEX_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Codex.hook_events(),
    },
    IntegrationSpec {
        target: Target::Copilot,
        config_files: &[super::COPILOT_SETTINGS_NAME],
        registration: RegistrationCheck::Json {
            root: HooksRoot::HooksKey,
            shape: JsonShape::Direct,
        },
        action_label: "copilot",
        install: super::targets::install_copilot,
        uninstall: super::targets::uninstall_copilot,
        assets: &[super::COPILOT_HOOK_ASSET],
        directory: DirectoryKey::Copilot,
        path: &["hooks", super::COPILOT_HOOK_INSTALL_NAME],
        version: super::COPILOT_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Copilot.hook_events(),
    },
    IntegrationSpec {
        target: Target::Devin,
        config_files: &[super::DEVIN_CONFIG_NAME],
        registration: RegistrationCheck::Json {
            root: HooksRoot::HooksKey,
            shape: JsonShape::Nested,
        },
        action_label: "devin",
        install: super::targets::install_devin,
        uninstall: super::targets::uninstall_devin,
        assets: &[super::DEVIN_HOOK_ASSET],
        directory: DirectoryKey::Devin,
        path: &[super::DEVIN_HOOK_INSTALL_NAME],
        version: super::DEVIN_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Devin.hook_events(),
    },
    IntegrationSpec {
        target: Target::Droid,
        config_files: &[super::DROID_SETTINGS_NAME],
        registration: RegistrationCheck::Json {
            root: HooksRoot::HooksKey,
            shape: JsonShape::Nested,
        },
        action_label: "droid",
        install: super::targets::install_droid,
        uninstall: super::targets::uninstall_droid,
        assets: &[super::DROID_HOOK_ASSET],
        directory: DirectoryKey::Droid,
        path: &["hooks", super::DROID_HOOK_INSTALL_NAME],
        version: super::DROID_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Droid.hook_events(),
    },
    IntegrationSpec {
        target: Target::Kimi,
        config_files: &[super::KIMI_CONFIG_NAME],
        registration: RegistrationCheck::Kimi,
        action_label: "kimi",
        install: super::targets::install_kimi,
        uninstall: super::targets::uninstall_kimi,
        assets: &[super::KIMI_HOOK_ASSET],
        directory: DirectoryKey::Kimi,
        path: &["hooks", super::KIMI_HOOK_INSTALL_NAME],
        version: super::KIMI_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Kimi.hook_events(),
    },
    IntegrationSpec {
        target: Target::Opencode,
        config_files: &[
            super::OPENCODE_TUI_CONFIG_NAME,
            super::OPENCODE_LEGACY_TUI_CONFIG_NAME,
            super::OPENCODE_CLI_CONFIG_NAME,
        ],
        registration: RegistrationCheck::Opencode,
        action_label: "opencode",
        install: super::targets::install_opencode,
        uninstall: super::targets::uninstall_opencode,
        assets: &[
            super::OPENCODE_PLUGIN_ASSET,
            super::OPENCODE_TUI_PLUGIN_ASSET,
            super::OPENCODE_V2_TUI_PLUGIN_ASSET,
        ],
        directory: DirectoryKey::Opencode,
        path: &["plugins", super::OPENCODE_PLUGIN_INSTALL_NAME],
        version: super::OPENCODE_INTEGRATION_VERSION,
        hook_timeout: None,
        events: Target::Opencode.hook_events(),
    },
    IntegrationSpec {
        target: Target::Kilo,
        config_files: &[],
        registration: RegistrationCheck::DirectoryLoaded,
        action_label: "kilo",
        install: super::targets::install_kilo,
        uninstall: super::targets::uninstall_kilo,
        assets: &[super::KILO_PLUGIN_ASSET],
        directory: DirectoryKey::Kilo,
        path: &["plugin", super::KILO_PLUGIN_INSTALL_NAME],
        version: super::KILO_INTEGRATION_VERSION,
        hook_timeout: None,
        events: Target::Kilo.hook_events(),
    },
    IntegrationSpec {
        target: Target::Qodercli,
        config_files: &[super::QODERCLI_SETTINGS_NAME],
        registration: RegistrationCheck::Json {
            root: HooksRoot::HooksKey,
            shape: JsonShape::NestedStar,
        },
        action_label: "qodercli",
        install: super::targets::install_qodercli,
        uninstall: super::targets::uninstall_qodercli,
        assets: &[super::QODERCLI_HOOK_ASSET],
        directory: DirectoryKey::Qodercli,
        path: &["hooks", super::QODERCLI_HOOK_INSTALL_NAME],
        version: super::QODERCLI_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Qodercli.hook_events(),
    },
    IntegrationSpec {
        target: Target::Qwen,
        config_files: &[super::QWEN_SETTINGS_NAME],
        registration: RegistrationCheck::Json {
            root: HooksRoot::HooksKey,
            shape: JsonShape::NestedStar,
        },
        action_label: "qwen",
        install: super::targets::install_qwen,
        uninstall: super::targets::uninstall_qwen,
        assets: &[super::QWEN_HOOK_ASSET],
        directory: DirectoryKey::Qwen,
        path: &["hooks", super::QWEN_HOOK_INSTALL_NAME],
        version: super::QWEN_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Qwen.hook_events(),
    },
    IntegrationSpec {
        target: Target::Cursor,
        config_files: &[super::CURSOR_HOOKS_NAME],
        registration: RegistrationCheck::Json {
            root: HooksRoot::HooksKey,
            shape: JsonShape::Simple,
        },
        action_label: "cursor",
        install: super::targets::install_cursor,
        uninstall: super::targets::uninstall_cursor,
        assets: &[super::CURSOR_HOOK_ASSET],
        directory: DirectoryKey::Cursor,
        path: &[super::CURSOR_HOOK_INSTALL_NAME],
        version: super::CURSOR_INTEGRATION_VERSION,
        hook_timeout: None,
        events: Target::Cursor.hook_events(),
    },
    IntegrationSpec {
        target: Target::Mastracode,
        config_files: &[super::MASTRACODE_HOOKS_NAME],
        registration: RegistrationCheck::Json {
            root: HooksRoot::Document,
            shape: JsonShape::Flat,
        },
        action_label: "mastracode",
        install: super::targets::install_mastracode,
        uninstall: super::targets::uninstall_mastracode,
        assets: &[super::MASTRACODE_HOOK_ASSET],
        directory: DirectoryKey::Mastracode,
        path: &["hooks", super::MASTRACODE_HOOK_INSTALL_NAME],
        version: super::MASTRACODE_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Mastracode.hook_events(),
    },
    IntegrationSpec {
        target: Target::AntigravityCli,
        config_files: &[super::ANTIGRAVITY_CLI_HOOKS_NAME],
        registration: RegistrationCheck::AntigravityCli,
        action_label: "antigravity-cli",
        install: super::targets::install_antigravity_cli,
        uninstall: super::targets::uninstall_antigravity_cli,
        assets: &[super::ANTIGRAVITY_CLI_HOOK_ASSET],
        directory: DirectoryKey::AntigravityCli,
        path: &["hooks", super::ANTIGRAVITY_CLI_HOOK_INSTALL_NAME],
        version: super::ANTIGRAVITY_CLI_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::AntigravityCli.hook_events(),
    },
    IntegrationSpec {
        target: Target::Grok,
        config_files: &[],
        registration: RegistrationCheck::Grok,
        action_label: "grok",
        install: super::targets::install_grok,
        uninstall: super::targets::uninstall_grok,
        assets: &[super::GROK_HOOK_ASSET],
        directory: DirectoryKey::Grok,
        path: &["hooks", super::GROK_HOOK_INSTALL_NAME],
        version: super::GROK_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Grok.hook_events(),
    },
    IntegrationSpec {
        target: Target::Letta,
        config_files: &[super::LETTA_SETTINGS_NAME],
        registration: RegistrationCheck::Json {
            root: HooksRoot::HooksKey,
            shape: JsonShape::Nested,
        },
        action_label: "letta",
        install: super::targets::install_letta,
        uninstall: super::targets::uninstall_letta,
        assets: &[super::LETTA_HOOK_ASSET],
        directory: DirectoryKey::Letta,
        path: &["hooks", super::LETTA_HOOK_INSTALL_NAME],
        version: super::LETTA_INTEGRATION_VERSION,
        hook_timeout: Some(super::HOOK_TIMEOUT),
        events: Target::Letta.hook_events(),
    },
];

fn spec_for(target: Target) -> io::Result<&'static IntegrationSpec> {
    INTEGRATION_SPECS
        .iter()
        .find(|spec| spec.target == target)
        .ok_or_else(|| io::Error::other(format!("missing integration spec for {target:?}")))
}

/// The agent-owned config files `target`'s install edits, as its spec row
/// declares them.
pub(crate) fn config_file_names(target: Target) -> io::Result<&'static [&'static str]> {
    spec_for(target).map(|spec| spec.config_files)
}

pub(crate) fn integration_hook_timeout(target: Target) -> io::Result<Duration> {
    spec_for(target)?.hook_timeout.ok_or_else(|| {
        io::Error::other(format!(
            "integration spec for {target:?} has no hook timeout"
        ))
    })
}

pub(crate) fn action_label(target: Target) -> &'static str {
    INTEGRATION_SPECS
        .iter()
        .find(|spec| spec.target == target)
        .map_or(target.label(), |spec| spec.action_label)
}

pub(crate) fn install_operation(
    paths: &AgentIntegrationPaths,
    target: Target,
) -> io::Result<InstallOutcome> {
    let Some(spec) = INTEGRATION_SPECS.iter().find(|spec| spec.target == target) else {
        return Err(io::Error::other(format!(
            "missing integration spec for {target:?}"
        )));
    };
    (spec.install)(paths)
}

pub(crate) fn uninstall_operation(
    paths: &AgentIntegrationPaths,
    target: Target,
) -> io::Result<UninstallOutcome> {
    let Some(spec) = INTEGRATION_SPECS.iter().find(|spec| spec.target == target) else {
        return Err(io::Error::other(format!(
            "missing integration spec for {target:?}"
        )));
    };
    (spec.uninstall)(paths)
}

pub(crate) fn integration_asset(target: crate::agent::IntegrationTarget) -> Option<&'static str> {
    INTEGRATION_SPECS
        .iter()
        .copied()
        .find(|spec| spec.target == target)
        .and_then(|spec| spec.assets.first().copied())
}

pub(crate) fn integration_hook_events(
    target: crate::agent::IntegrationTarget,
) -> &'static [crate::agent::IntegrationHookEvent] {
    INTEGRATION_SPECS
        .iter()
        .find(|spec| spec.target == target)
        .map_or(&[], |spec| spec.events)
}

/// One row per supported target, in spec order, for `integration status`.
/// Includes `NotInstalled` rows because the command reports the full
/// supported-target inventory. A target whose directory could not be resolved,
/// or whose installed file could not be stat'ed, is an error row, so the CLI
/// can print it instead of silently omitting it.
pub fn integration_status_rows(
    paths: &super::env::AgentIntegrationPaths,
) -> Vec<Result<super::IntegrationStatus, super::IntegrationStatusError>> {
    integration_specs(paths)
        .map(|(target, path, expected_version)| {
            path.and_then(|path| integration_status_at(target, path, expected_version))
                .map_err(|error| super::IntegrationStatusError {
                    target,
                    message: error.to_string(),
                })
        })
        .collect()
}

/// The resolvable rows of [`integration_status_rows`]. Targets that could not
/// be checked are logged and skipped: callers here only act on installed
/// integrations.
pub(crate) fn installed_integration_statuses(
    paths: &super::env::AgentIntegrationPaths,
) -> Vec<super::IntegrationStatus> {
    integration_status_rows(paths)
        .into_iter()
        .filter_map(|row| match row {
            Ok(status) => Some(status),
            Err(error) => {
                tracing::warn!(
                    integration = error.target.label(),
                    error = %error.message,
                    "could not check integration status"
                );
                None
            }
        })
        .collect()
}

pub(crate) fn outdated_installed_integrations(
    paths: &super::env::AgentIntegrationPaths,
) -> Vec<super::IntegrationStatus> {
    installed_integration_statuses(paths)
        .into_iter()
        .filter(|status| status.state == super::IntegrationStatusKind::Outdated)
        .collect()
}

fn integration_specs(
    paths: &super::env::AgentIntegrationPaths,
) -> impl Iterator<Item = (crate::agent::IntegrationTarget, io::Result<PathBuf>, u32)> + '_ {
    INTEGRATION_SPECS.iter().copied().map(move |spec| {
        let path = paths.directory(spec.directory).map(|mut path| {
            for part in spec.path {
                path.push(part);
            }
            path
        });
        (spec.target, path, spec.version)
    })
}

pub(crate) fn integration_update_instructions(
    targets: &[crate::agent::IntegrationTarget],
) -> String {
    let commands: Vec<String> = targets
        .iter()
        .map(|target| {
            format!(
                "`shepr integration install {}`",
                integration_target_label(*target)
            )
        })
        .collect();

    match commands.as_slice() {
        [] => String::new(),
        [command] => format!("run {command}"),
        [rest @ .., last] => format!("run {} and {last}", rest.join(", ")),
    }
}

/// The operator notice for outdated installed integrations, or `None` when
/// every installed integration is current. The caller decides where it goes.
pub fn outdated_update_notice(paths: &super::env::AgentIntegrationPaths) -> Option<String> {
    let outdated = outdated_installed_integrations(paths);
    if outdated.is_empty() {
        return None;
    }

    let targets = outdated
        .iter()
        .map(|integration| integration.target)
        .collect::<Vec<_>>();
    Some(format!(
        "installed shepr integrations need updating; {}.",
        integration_update_instructions(&targets).replace('`', "")
    ))
}

/// Whether the Shepr-owned Grok hook config exactly matches the installed
/// integration. JSON formatting and object key order do not affect validity.
fn grok_hook_config_is_valid(hook_path: &Path) -> bool {
    let Some(hooks_dir) = hook_path.parent() else {
        return false;
    };
    let Ok(expected_config) = super::targets::grok_hook_config(hook_path) else {
        return false;
    };
    let config_path = hooks_dir.join(super::GROK_HOOK_CONFIG_NAME);
    fs::read_to_string(config_path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .is_some_and(|config| config == expected_config)
}

fn opencode_tui_integration_is_valid(
    plugin_path: &Path,
    expected_version: u32,
) -> io::Result<bool> {
    let Some(config_dir) = plugin_path.parent().and_then(Path::parent) else {
        return Ok(false);
    };
    let tui_plugin_path = config_dir.join(super::OPENCODE_TUI_PLUGIN_INSTALL_NAME);
    let tui_plugin_current = fs::read_to_string(tui_plugin_path)
        .ok()
        .and_then(|content| parse_integration_version(&content))
        .is_some_and(|version| version >= expected_version);
    let cli_config_path = config_dir.join(super::OPENCODE_CLI_CONFIG_NAME);
    let cli_config_exists = cli_config_path.try_exists().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot stat {}: {error}", cli_config_path.display()),
        )
    })?;
    Ok(tui_plugin_current
        && super::opencode_config::tui_plugin_is_configured(
            config_dir,
            super::OPENCODE_TUI_PLUGIN_SPEC,
        )
        && (!cli_config_exists
            || (super::opencode_config::cli_plugin_is_configured(
                config_dir,
                super::OPENCODE_V2_TUI_PLUGIN_SPEC,
            ) && fs::read_to_string(
                config_dir
                    .join(super::OPENCODE_V2_TUI_PLUGIN_DIR)
                    .join("tui.js"),
            )
            .ok()
            .and_then(|content| parse_integration_version(&content))
            .is_some_and(|version| version >= expected_version))))
}

/// `levels` directories up from `path` (1 is the parent).
fn ancestor(path: &Path, levels: usize) -> Option<&Path> {
    let mut current = path;
    for _ in 0..levels {
        current = current.parent()?;
    }
    Some(current)
}

#[derive(Clone, Copy)]
enum HooksRoot {
    /// Events live under the document's top-level `hooks` object.
    HooksKey,
    /// Events are the document's own top-level keys (MastraCode).
    Document,
}

enum JsonHookShape {
    // `None` means the installer wrote no matcher field on the event group.
    Nested { matcher: Option<String> },
    Flat,
    Direct,
    Simple,
}

/// Whether `entries` holds the hook in the shape its installer writes, under
/// the installer's matcher. Install first strips every entry carrying shepr's
/// command from the event, whatever its matcher or extra fields, and then
/// writes the canonical one, so anything this rejects a reinstall repairs.
///
/// There is deliberately no per-entry `disabled` or `enabled` check: none of
/// these agents documents such a field (Claude Code and Qwen Code only offer
/// the global `disableAllHooks`, Cursor has neither), so an entry carrying one
/// still runs and still counts as registered.
fn json_event_has_command(
    entries: &serde_json::Value,
    command: &str,
    shape: &JsonHookShape,
) -> bool {
    let Some(entries) = entries.as_array() else {
        return false;
    };
    match shape {
        JsonHookShape::Nested { matcher } => entries.iter().any(|group| {
            let matcher_matches = match matcher {
                Some(matcher) => {
                    group.get("matcher").and_then(serde_json::Value::as_str)
                        == Some(matcher.as_str())
                }
                None => group.get("matcher").is_none(),
            };
            matcher_matches
                && group
                    .get("hooks")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|hooks| {
                        hooks
                            .iter()
                            .any(|hook| is_matching_command_hook(hook, command))
                    })
        }),
        JsonHookShape::Flat => entries
            .iter()
            .any(|hook| hook.get("matcher").is_none() && is_matching_command_hook(hook, command)),
        JsonHookShape::Direct => entries.iter().any(|hook| {
            hook.get("matcher").is_none()
                && hook.get("type").and_then(serde_json::Value::as_str) == Some("command")
                && hook
                    .get(direct_command_field())
                    .and_then(serde_json::Value::as_str)
                    == Some(command)
        }),
        JsonHookShape::Simple => entries.iter().any(|hook| {
            hook.get("matcher").is_none()
                && hook.get("command").and_then(serde_json::Value::as_str) == Some(command)
        }),
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Every `(event, command)` pair appears in the command field used by that
/// target's installer shape, under the matching event.
fn json_hook_commands_registered(
    config_path: &Path,
    root: HooksRoot,
    expected: &[(&str, String)],
    shape: &JsonHookShape,
) -> bool {
    let Some(document) = read_json(config_path) else {
        return false;
    };
    let events = match root {
        HooksRoot::HooksKey => document.get("hooks"),
        HooksRoot::Document => Some(&document),
    };
    let Some(events) = events.and_then(serde_json::Value::as_object) else {
        return false;
    };
    expected.iter().all(|(event, command)| {
        events
            .get(*event)
            .is_some_and(|entries| json_event_has_command(entries, command, shape))
    })
}

fn read_toml(path: &Path) -> Option<toml::Value> {
    toml::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn codex_hooks_feature_enabled(config_path: &Path) -> bool {
    read_toml(config_path).and_then(|config| config.get("features")?.get("hooks")?.as_bool())
        == Some(true)
}

fn kimi_hooks_registered(config_path: &Path, hook_path: &Path) -> bool {
    let Some(config) = read_toml(config_path) else {
        return false;
    };
    let Some(entries) = config.get("hooks").and_then(toml::Value::as_array) else {
        return false;
    };
    integration_hook_events(crate::agent::IntegrationTarget::Kimi)
        .iter()
        .all(|hook| {
            hook.action.is_some_and(|action| {
                let command = hook_command(hook_path, Some(action.as_str()));
                entries.iter().any(|entry| {
                    entry.get("event").and_then(toml::Value::as_str) == Some(hook.event)
                        && entry.get("command").and_then(toml::Value::as_str)
                            == Some(command.as_str())
                        && entry.get("matcher").and_then(toml::Value::as_str) == hook.matcher
                })
            })
        })
}

/// Convert an agent's hook events into the commands its integration registers.
fn hook_event_commands(
    hook_path: &Path,
    events: &[crate::agent::IntegrationHookEvent],
) -> Vec<(&'static str, String)> {
    events
        .iter()
        .filter_map(|hook| {
            hook.action
                .map(|action| (hook.event, hook_command(hook_path, Some(action.as_str()))))
        })
        .collect()
}

/// Whether the agent's own config still registers the installed hook, the
/// way install wrote it. The hook file's version marker alone cannot tell: an
/// install whose config edit failed, or a user who deleted the settings entry,
/// leaves a current hook script the agent never runs.
///
/// The config files are read from the directory the spec row's `path` is
/// installed under, so the depth follows the row instead of a hand-kept count.
fn hook_registration_is_current(
    spec: &IntegrationSpec,
    hook_path: &Path,
    expected_version: u32,
) -> io::Result<bool> {
    let Some(dir) = ancestor(hook_path, spec.path.len()) else {
        return Ok(false);
    };
    let config = |index: usize| {
        spec.config_files
            .get(index)
            .map(|name| dir.join(name))
            .ok_or_else(|| {
                io::Error::other(format!(
                    "integration spec for {:?} declares no config file {index} for its registration check",
                    spec.target
                ))
            })
    };
    let registered = match spec.registration {
        RegistrationCheck::DirectoryLoaded => true,
        RegistrationCheck::Grok => grok_hook_config_is_valid(hook_path),
        RegistrationCheck::Opencode => {
            return opencode_tui_integration_is_valid(hook_path, expected_version);
        }
        RegistrationCheck::Kimi => kimi_hooks_registered(&config(0)?, hook_path),
        RegistrationCheck::AntigravityCli => {
            let expected_block = super::targets::antigravity_cli_hook_block(hook_path)?;
            read_json(&config(0)?).is_some_and(|document| {
                document.get(super::ANTIGRAVITY_CLI_HOOK_BLOCK_NAME) == Some(&expected_block)
            })
        }
        RegistrationCheck::Codex => {
            json_hook_commands_registered(
                &config(0)?,
                HooksRoot::HooksKey,
                &hook_event_commands(hook_path, spec.events),
                &JsonHookShape::Nested { matcher: None },
            ) && codex_hooks_feature_enabled(&config(1)?)
        }
        RegistrationCheck::Json { root, shape } => {
            let expected = match shape {
                // A direct entry is written for every event, including the
                // ones whose hook takes no action argument.
                JsonShape::Direct => spec
                    .events
                    .iter()
                    .map(|hook| {
                        (
                            hook.event,
                            hook_command(
                                hook_path,
                                hook.action.map(crate::agent::IntegrationHookAction::as_str),
                            ),
                        )
                    })
                    .collect::<Vec<_>>(),
                _ => hook_event_commands(hook_path, spec.events),
            };
            let shape = match shape {
                JsonShape::Nested => JsonHookShape::Nested { matcher: None },
                JsonShape::NestedStar => JsonHookShape::Nested {
                    matcher: Some("*".to_owned()),
                },
                JsonShape::NestedClaude => JsonHookShape::Nested {
                    matcher: Some(super::claude_settings::claude_session_start_matcher()),
                },
                JsonShape::Flat => JsonHookShape::Flat,
                JsonShape::Direct => JsonHookShape::Direct,
                JsonShape::Simple => JsonHookShape::Simple,
            };
            json_hook_commands_registered(&config(0)?, root, &expected, &shape)
        }
    };
    Ok(registered)
}

fn integration_state_for_path(
    path: &Path,
    expected_version: u32,
) -> io::Result<(super::IntegrationStatusKind, Option<u32>)> {
    let installed = super::file_ops::is_file(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot stat {}: {error}", path.display()),
        )
    })?;
    if !installed {
        return Ok((super::IntegrationStatusKind::NotInstalled, None));
    }

    let installed_version = fs::read_to_string(path)
        .ok()
        .and_then(|content| parse_integration_version(&content));
    let state = if installed_version.is_some_and(|version| version >= expected_version) {
        super::IntegrationStatusKind::Current
    } else {
        super::IntegrationStatusKind::Outdated
    };

    Ok((state, installed_version))
}

/// The status of the integration installed at `path`. A stat error on the
/// installed file (or on a file its validity depends on) is returned, not
/// reported as `NotInstalled`.
pub(crate) fn integration_status_at(
    target: crate::agent::IntegrationTarget,
    path: PathBuf,
    expected_version: u32,
) -> io::Result<super::IntegrationStatus> {
    let (mut state, installed_version) = integration_state_for_path(&path, expected_version)?;

    if state == super::IntegrationStatusKind::Current
        && !hook_registration_is_current(spec_for(target)?, &path, expected_version)?
    {
        state = super::IntegrationStatusKind::Outdated;
    }

    Ok(super::IntegrationStatus {
        target,
        path,
        state,
        installed_version,
        expected_version,
    })
}

pub(crate) fn parse_integration_version(content: &str) -> Option<u32> {
    content.lines().find_map(|line| {
        let marker_line = line
            .trim()
            .trim_start_matches('/')
            .trim_start_matches('#')
            .trim();
        marker_line
            .strip_prefix(super::INTEGRATION_VERSION_MARKER)?
            .trim()
            .parse()
            .ok()
    })
}

#[cfg(test)]
mod registration_tests {
    use super::*;
    use crate::agent::IntegrationTarget;
    use crate::integration::IntegrationStatusKind;

    #[test]
    fn antigravity_integration_uses_the_canonical_agent_label() {
        assert_eq!(
            integration_target_label(IntegrationTarget::AntigravityCli),
            crate::agent::Agent::Antigravity.label()
        );
    }

    #[test]
    fn bundled_integration_assets_match_expected_versions() {
        for spec in INTEGRATION_SPECS {
            assert!(
                !spec.assets.is_empty(),
                "{} must register its bundled assets",
                spec.target.label()
            );
            for (index, asset) in spec.assets.iter().enumerate() {
                assert_eq!(
                    parse_integration_version(asset),
                    Some(spec.version),
                    "{} bundled asset {index} must match its integration version",
                    spec.target.label()
                );
            }
        }
    }

    #[test]
    fn bundled_integration_assets_report_the_descriptor_identity() {
        for spec in INTEGRATION_SPECS {
            let agent = spec.target.agent();
            let source = agent
                .integration_source()
                .expect("integration targets must have a source");
            for (index, asset) in spec.assets.iter().enumerate() {
                // OpenCode V2 re-exports the TUI reporter, so its identity lives in that asset.
                if spec.target == Target::Opencode
                    && *asset == super::super::OPENCODE_V2_TUI_PLUGIN_ASSET
                {
                    continue;
                }
                assert!(
                    asset.contains(source),
                    "{} bundled asset {index} must report source {source:?}",
                    agent.label()
                );
                assert!(
                    asset.contains(agent.label()),
                    "{} bundled asset {index} must report its canonical label",
                    agent.label()
                );
            }
        }
    }

    #[test]
    fn unresolvable_integration_directories_are_reported_as_error_rows() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.remove("HOME");
        let paths = crate::integration::AgentIntegrationPaths::resolve();

        let rows = integration_status_rows(&paths);
        assert_eq!(rows.len(), INTEGRATION_SPECS.len(), "one row per target");
        let errors = rows.iter().filter(|row| row.is_err()).count();
        assert!(errors > 0, "targets under HOME cannot resolve without it");
        assert_eq!(
            installed_integration_statuses(&paths).len(),
            rows.len() - errors
        );
    }

    fn base(name: &str) -> PathBuf {
        shepr_test_support::ScratchDir::new(name).to_path_buf()
    }

    fn write_current_hook(path: &Path) {
        fs::create_dir_all(path.parent().expect("test precondition")).expect("test precondition");
        fs::write(
            path,
            format!("# {}1\n", super::super::INTEGRATION_VERSION_MARKER),
        )
        .expect("test precondition");
    }

    fn state(target: IntegrationTarget, hook: &Path) -> IntegrationStatusKind {
        integration_status_at(target, hook.to_path_buf(), 1)
            .expect("stat hook")
            .state
    }

    #[test]
    fn claude_hook_without_settings_entry_is_outdated() {
        let dir = base("claude");
        let hook = dir.join("hooks").join("shepr-agent-state.sh");
        write_current_hook(&hook);
        assert_eq!(
            state(IntegrationTarget::Claude, &hook),
            IntegrationStatusKind::Outdated
        );

        let settings_path = dir.join("settings.json");
        let target = Target::Claude;
        let installed = super::super::claude_settings::install(
            "{}",
            &settings_path,
            &hook,
            integration_hook_events(target),
            integration_hook_timeout(target).expect("test precondition"),
        )
        .expect("test precondition");
        fs::write(&settings_path, installed).expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Claude, &hook),
            IntegrationStatusKind::Current
        );

        // The user deleting the entry leaves the hook file current but inert.
        fs::write(&settings_path, "{\"hooks\":{}}").expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Claude, &hook),
            IntegrationStatusKind::Outdated
        );
        fs::write(&settings_path, "{ not json").expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Claude, &hook),
            IntegrationStatusKind::Outdated
        );
    }

    #[test]
    fn claude_command_outside_the_installed_shape_is_outdated() {
        let dir = base("claude-shape");
        let hook = dir.join("hooks").join("shepr-agent-state.sh");
        write_current_hook(&hook);
        let settings_path = dir.join("settings.json");
        let command = hook_command(&hook, Some("session"));
        let matcher = super::super::claude_settings::claude_session_start_matcher();
        let write = |session_start: serde_json::Value| {
            let settings = serde_json::json!({ "hooks": { "SessionStart": session_start } });
            fs::write(&settings_path, settings.to_string()).expect("test precondition");
        };

        write(serde_json::json!([
            { "matcher": matcher, "hooks": [{ "type": "command", "command": command }] }
        ]));
        assert_eq!(
            state(IntegrationTarget::Claude, &hook),
            IntegrationStatusKind::Current
        );

        // Claude has no per-hook disable switch, so an unknown `disabled` field
        // does not stop the hook running and does not unregister it.
        write(serde_json::json!([{ "matcher": matcher, "hooks": [
            { "type": "command", "command": command, "disabled": true }
        ] }]));
        assert_eq!(
            state(IntegrationTarget::Claude, &hook),
            IntegrationStatusKind::Current
        );

        for session_start in [
            // An unrelated field of another hook carries the command string.
            serde_json::json!([{ "matcher": matcher, "hooks": [
                { "type": "command", "command": "echo keep", "description": command }
            ] }]),
            // The right hook under a group with another matcher.
            serde_json::json!([{ "matcher": "startup", "hooks": [
                { "type": "command", "command": command }
            ] }]),
        ] {
            write(session_start.clone());
            assert_eq!(
                state(IntegrationTarget::Claude, &hook),
                IntegrationStatusKind::Outdated,
                "{session_start}"
            );
        }
    }

    #[test]
    fn codex_needs_the_hooks_entry_and_the_feature_flag() {
        let dir = base("codex");
        let hook = dir.join("shepr-agent-state.sh");
        write_current_hook(&hook);
        let hooks_json = serde_json::json!({
            "hooks": { "SessionStart": [
                { "hooks": [{ "type": "command", "command": hook_command(&hook, Some("session")), "timeout": 10 }] }
            ] }
        });
        fs::write(dir.join("hooks.json"), hooks_json.to_string()).expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Codex, &hook),
            IntegrationStatusKind::Outdated
        );
        fs::write(
            dir.join("config.toml"),
            "model = \"x\"\n[features]\nhooks = true\n",
        )
        .expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Codex, &hook),
            IntegrationStatusKind::Current
        );
        fs::write(dir.join("config.toml"), "features.hooks = false\n").expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Codex, &hook),
            IntegrationStatusKind::Outdated
        );
    }

    #[test]
    fn kimi_needs_every_hook_table() {
        let dir = base("kimi");
        let hook = dir.join("hooks").join("shepr-agent-state.sh");
        write_current_hook(&hook);
        let config_path = dir.join("config.toml");
        let config = super::super::config_edit::build_kimi_config_with_hooks("", &hook)
            .expect("test precondition");
        fs::write(&config_path, &config).expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Kimi, &hook),
            IntegrationStatusKind::Current
        );
        // Drop the last hook table: the registration is incomplete.
        let truncated = config
            .rsplit_once("[[hooks]]")
            .map(|(head, _)| head.to_string())
            .expect("test precondition");
        fs::write(&config_path, truncated).expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Kimi, &hook),
            IntegrationStatusKind::Outdated
        );
    }

    #[test]
    fn mastracode_checks_flat_top_level_events() {
        let dir = base("mastracode");
        let hook = dir.join("hooks").join("shepr-agent-state.sh");
        write_current_hook(&hook);
        let mut document = serde_json::Map::new();
        for event_spec in integration_hook_events(IntegrationTarget::Mastracode) {
            let Some(action) = event_spec.action else {
                continue;
            };
            document.insert(
                event_spec.event.to_string(),
                serde_json::json!([{ "type": "command", "command": hook_command(&hook, Some(action.as_str())) }]),
            );
        }
        fs::write(
            dir.join("hooks.json"),
            serde_json::Value::Object(document.clone()).to_string(),
        )
        .expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Mastracode, &hook),
            IntegrationStatusKind::Current
        );
        document.remove("Stop");
        fs::write(
            dir.join("hooks.json"),
            serde_json::Value::Object(document).to_string(),
        )
        .expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Mastracode, &hook),
            IntegrationStatusKind::Outdated
        );
    }

    #[test]
    fn install_with_unparseable_config_writes_no_hook() {
        let env = shepr_test_support::IsolatedEnv::new();

        let claude = base("claude-malformed");
        fs::write(claude.join("settings.json"), "{ not json").expect("test precondition");
        env.set("CLAUDE_CONFIG_DIR", &claude);
        let result = super::super::targets::install_claude(
            &super::super::env::AgentIntegrationPaths::resolve(),
        );
        assert!(result.is_err());
        assert!(
            !claude
                .join("hooks")
                .join(super::super::CLAUDE_HOOK_INSTALL_NAME)
                .try_exists()
                .expect("stat hook")
        );

        let codex = base("codex-malformed");
        fs::write(codex.join("hooks.json"), "[1,").expect("test precondition");
        env.set("CODEX_HOME", &codex);
        let result = super::super::targets::install_codex(
            &super::super::env::AgentIntegrationPaths::resolve(),
        );
        assert!(result.is_err());
        assert!(
            !codex
                .join(super::super::CODEX_HOOK_INSTALL_NAME)
                .try_exists()
                .expect("stat hook")
        );

        let copilot = base("copilot-malformed");
        fs::write(copilot.join("settings.json"), "{\"hooks\": []}").expect("test precondition");
        env.set("COPILOT_HOME", &copilot);
        let result = super::super::targets::install_copilot(
            &super::super::env::AgentIntegrationPaths::resolve(),
        );
        assert!(result.is_err());
        assert!(!copilot.join("hooks").try_exists().expect("stat hooks dir"));
    }

    /// For every JSON-registered target: a fresh install reads Current, a
    /// hand-edited entry (moved under another matcher) reads Outdated, and a
    /// reinstall repairs it back to Current. An unknown `disabled` field does
    /// not unregister the hook, since none of these agents honours one.
    #[test]
    fn install_repairs_every_registration_status_rejects() {
        use super::super::targets;

        type Install = fn(&super::super::env::AgentIntegrationPaths) -> io::Result<()>;
        let env = shepr_test_support::IsolatedEnv::new();
        let home = env.home();
        let cases: [(IntegrationTarget, &[&str], &str, HooksRoot, Install); 10] = [
            (
                IntegrationTarget::Claude,
                &[".claude"],
                "settings.json",
                HooksRoot::HooksKey,
                |paths| targets::install_claude(paths).map(|_| ()),
            ),
            (
                IntegrationTarget::Codex,
                &[".codex"],
                "hooks.json",
                HooksRoot::HooksKey,
                |paths| targets::install_codex(paths).map(|_| ()),
            ),
            (
                IntegrationTarget::Copilot,
                &[".copilot"],
                "settings.json",
                HooksRoot::HooksKey,
                |paths| targets::install_copilot(paths).map(|_| ()),
            ),
            (
                IntegrationTarget::Devin,
                &[".config", "devin"],
                "config.json",
                HooksRoot::HooksKey,
                |paths| targets::install_devin(paths).map(|_| ()),
            ),
            (
                IntegrationTarget::Droid,
                &[".factory"],
                "settings.json",
                HooksRoot::HooksKey,
                |paths| targets::install_droid(paths).map(|_| ()),
            ),
            (
                IntegrationTarget::Qodercli,
                &[".qoder"],
                "settings.json",
                HooksRoot::HooksKey,
                |paths| targets::install_qodercli(paths).map(|_| ()),
            ),
            (
                IntegrationTarget::Qwen,
                &[".qwen"],
                "settings.json",
                HooksRoot::HooksKey,
                |paths| targets::install_qwen(paths).map(|_| ()),
            ),
            (
                IntegrationTarget::Letta,
                &[".letta"],
                "settings.json",
                HooksRoot::HooksKey,
                |paths| targets::install_letta(paths).map(|_| ()),
            ),
            (
                IntegrationTarget::Cursor,
                &[".cursor"],
                "hooks.json",
                HooksRoot::HooksKey,
                |paths| targets::install_cursor(paths).map(|_| ()),
            ),
            (
                IntegrationTarget::Mastracode,
                &[".mastracode"],
                "hooks.json",
                HooksRoot::Document,
                |paths| targets::install_mastracode(paths).map(|_| ()),
            ),
        ];

        for (target, dir_parts, config_name, root, install) in cases {
            let dir = dir_parts
                .iter()
                .fold(home.clone(), |dir, part| dir.join(part));
            fs::create_dir_all(&dir).expect("test precondition");
            let paths = super::super::env::AgentIntegrationPaths::resolve();
            let status = || {
                integration_status_rows(&paths)
                    .into_iter()
                    .filter_map(Result::ok)
                    .find(|status| status.target == target)
                    .expect("status row")
            };
            let config_path = dir.join(config_name);
            // Apply `edit` to every event entry that carries this hook.
            let edit_entries = |edit: &dyn Fn(&mut serde_json::Map<String, serde_json::Value>)| {
                let hook = status().path.display().to_string();
                let mut document = read_json(&config_path).expect("test precondition");
                let events = match root {
                    HooksRoot::HooksKey => document.get_mut("hooks"),
                    HooksRoot::Document => Some(&mut document),
                }
                .and_then(serde_json::Value::as_object_mut)
                .expect("test precondition");
                for entries in events
                    .values_mut()
                    .filter_map(serde_json::Value::as_array_mut)
                {
                    for entry in entries {
                        if entry.to_string().contains(&hook) {
                            edit(entry.as_object_mut().expect("test precondition"));
                        }
                    }
                }
                fs::write(&config_path, document.to_string()).expect("test precondition");
            };

            install(&paths).expect("install");
            assert_eq!(status().state, IntegrationStatusKind::Current, "{target:?}");

            edit_entries(&|entry| {
                entry.insert("disabled".to_owned(), serde_json::Value::Bool(true));
            });
            assert_eq!(status().state, IntegrationStatusKind::Current, "{target:?}");

            edit_entries(&|entry| {
                entry.insert("matcher".to_owned(), "hand-edited".into());
            });
            assert_eq!(
                status().state,
                IntegrationStatusKind::Outdated,
                "{target:?}"
            );

            install(&paths).expect("reinstall");
            assert_eq!(status().state, IntegrationStatusKind::Current, "{target:?}");
            let document = fs::read_to_string(&config_path).expect("test precondition");
            assert!(!document.contains("hand-edited"), "{target:?}: {document}");
        }
    }

    #[test]
    fn letta_is_listed_and_needs_its_settings_entry() {
        let dir = base("letta");
        let hook = dir
            .join("hooks")
            .join(super::super::LETTA_HOOK_INSTALL_NAME);
        write_current_hook(&hook);
        assert_eq!(
            state(IntegrationTarget::Letta, &hook),
            IntegrationStatusKind::Outdated
        );
        let settings = serde_json::json!({
            "hooks": { "SessionStart": [
                { "hooks": [{ "type": "command", "command": hook_command(&hook, Some("session")) }] }
            ] }
        });
        fs::write(dir.join("settings.json"), settings.to_string()).expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Letta, &hook),
            IntegrationStatusKind::Current
        );
        fs::write(dir.join("settings.json"), "{\"hooks\":{}}").expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Letta, &hook),
            IntegrationStatusKind::Outdated
        );
        assert_eq!(integration_target_label(IntegrationTarget::Letta), "letta");
    }

    #[test]
    fn every_target_has_exactly_one_status_spec() {
        let paths = super::super::env::AgentIntegrationPaths::resolve();
        let specs = integration_specs(&paths).collect::<Vec<_>>();
        for target in IntegrationTarget::all() {
            assert_eq!(
                specs.iter().filter(|(spec, _, _)| *spec == target).count(),
                1,
                "{target:?}"
            );
        }
    }

    #[test]
    fn plugin_directory_targets_need_only_the_file() {
        let dir = base("pi");
        let plugin = dir.join("extensions").join("shepr-agent-state.ts");
        write_current_hook(&plugin);
        assert_eq!(
            state(IntegrationTarget::Pi, &plugin),
            IntegrationStatusKind::Current
        );
    }
}
