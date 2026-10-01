use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::IntegrationTarget as Target;

use super::command::{hook_command, is_hook_command_for_path};
use super::config_edit::{direct_command_field, is_matching_command_hook};
use super::env::{AgentIntegrationPaths, DirectoryKey};
use super::types::InstallOutcome;

#[derive(Clone, Copy)]
struct IntegrationSpec {
    target: Target,
    assets: &'static [&'static str],
    directory: DirectoryKey,
    /// Agent-owned config files in `directory` that install edits. Install
    /// vets them before touching anything, and the registration check reads
    /// them from the directory `path` is installed under.
    config_files: &'static [&'static str],
    /// How status confirms the agent's own config still runs the hook.
    registration: RegistrationCheck,
    path: &'static [&'static str],
    hook_timeout: Option<Duration>,
    // Operator-facing label for install messages. For Antigravity this names
    // the CLI integration while the agent's internal label is `agy`.
    action_label: &'static str,
    install: fn(&AgentIntegrationPaths) -> io::Result<InstallOutcome>,
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
        assets: &[super::PI_EXTENSION_ASSET],
        directory: DirectoryKey::PiExtension,
        path: &[super::PI_EXTENSION_INSTALL_NAME],
        hook_timeout: None,
    },
    IntegrationSpec {
        target: Target::Omp,
        config_files: &[],
        registration: RegistrationCheck::DirectoryLoaded,
        action_label: "omp",
        install: super::targets::install_omp,
        assets: &[super::OMP_EXTENSION_ASSET],
        directory: DirectoryKey::OmpExtension,
        path: &[super::OMP_EXTENSION_INSTALL_NAME],
        hook_timeout: None,
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
        assets: &[super::CLAUDE_HOOK_ASSET],
        directory: DirectoryKey::Claude,
        path: &["hooks", super::CLAUDE_HOOK_INSTALL_NAME],
        hook_timeout: Some(super::HOOK_TIMEOUT),
    },
    IntegrationSpec {
        target: Target::Codex,
        config_files: &[super::CODEX_HOOKS_NAME, super::CODEX_CONFIG_NAME],
        registration: RegistrationCheck::Codex,
        action_label: "codex",
        install: super::targets::install_codex,
        assets: &[super::CODEX_HOOK_ASSET],
        directory: DirectoryKey::Codex,
        path: &[super::CODEX_HOOK_INSTALL_NAME],
        hook_timeout: Some(super::HOOK_TIMEOUT),
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
        assets: &[super::COPILOT_HOOK_ASSET],
        directory: DirectoryKey::Copilot,
        path: &["hooks", super::COPILOT_HOOK_INSTALL_NAME],
        hook_timeout: Some(super::HOOK_TIMEOUT),
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
        assets: &[super::DEVIN_HOOK_ASSET],
        directory: DirectoryKey::Devin,
        path: &[super::DEVIN_HOOK_INSTALL_NAME],
        hook_timeout: Some(super::HOOK_TIMEOUT),
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
        assets: &[super::DROID_HOOK_ASSET],
        directory: DirectoryKey::Droid,
        path: &["hooks", super::DROID_HOOK_INSTALL_NAME],
        hook_timeout: Some(super::HOOK_TIMEOUT),
    },
    IntegrationSpec {
        target: Target::Kimi,
        config_files: &[super::KIMI_CONFIG_NAME],
        registration: RegistrationCheck::Kimi,
        action_label: "kimi",
        install: super::targets::install_kimi,
        assets: &[super::KIMI_HOOK_ASSET],
        directory: DirectoryKey::Kimi,
        path: &["hooks", super::KIMI_HOOK_INSTALL_NAME],
        hook_timeout: Some(super::HOOK_TIMEOUT),
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
        assets: &[
            super::OPENCODE_PLUGIN_ASSET,
            super::OPENCODE_TUI_PLUGIN_ASSET,
            super::OPENCODE_V2_TUI_PLUGIN_ASSET,
        ],
        directory: DirectoryKey::Opencode,
        path: &["plugins", super::OPENCODE_PLUGIN_INSTALL_NAME],
        hook_timeout: None,
    },
    IntegrationSpec {
        target: Target::Kilo,
        config_files: &[],
        registration: RegistrationCheck::DirectoryLoaded,
        action_label: "kilo",
        install: super::targets::install_kilo,
        assets: &[super::KILO_PLUGIN_ASSET],
        directory: DirectoryKey::Kilo,
        path: &["plugin", super::KILO_PLUGIN_INSTALL_NAME],
        hook_timeout: None,
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
        assets: &[super::CURSOR_HOOK_ASSET],
        directory: DirectoryKey::Cursor,
        path: &[super::CURSOR_HOOK_INSTALL_NAME],
        hook_timeout: None,
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
        assets: &[super::MASTRACODE_HOOK_ASSET],
        directory: DirectoryKey::Mastracode,
        path: &["hooks", super::MASTRACODE_HOOK_INSTALL_NAME],
        hook_timeout: Some(super::HOOK_TIMEOUT),
    },
    IntegrationSpec {
        target: Target::AntigravityCli,
        config_files: &[super::ANTIGRAVITY_CLI_HOOKS_NAME],
        registration: RegistrationCheck::AntigravityCli,
        action_label: "antigravity-cli",
        install: super::targets::install_antigravity_cli,
        assets: &[super::ANTIGRAVITY_CLI_HOOK_ASSET],
        directory: DirectoryKey::AntigravityCli,
        path: &["hooks", super::ANTIGRAVITY_CLI_HOOK_INSTALL_NAME],
        hook_timeout: Some(super::HOOK_TIMEOUT),
    },
    IntegrationSpec {
        target: Target::Grok,
        config_files: &[],
        registration: RegistrationCheck::Grok,
        action_label: "grok",
        install: super::targets::install_grok,
        assets: &[super::GROK_HOOK_ASSET],
        directory: DirectoryKey::Grok,
        path: &["hooks", super::GROK_HOOK_INSTALL_NAME],
        hook_timeout: Some(super::HOOK_TIMEOUT),
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
        .map_or(&[], |spec| spec.target.hook_events())
}

/// The primary managed file `spec` installs, whose bundled bytes status checks.
fn installed_path(paths: &AgentIntegrationPaths, spec: &IntegrationSpec) -> io::Result<PathBuf> {
    let mut path = paths.directory(spec.directory)?;
    for part in spec.path {
        path.push(part);
    }
    Ok(path)
}

/// The status of `target`'s integration on this host. A directory that could
/// not be resolved, or an installed file that could not be stat'ed, is an
/// error, not `NotInstalled`.
pub(crate) fn integration_status(
    paths: &AgentIntegrationPaths,
    target: Target,
) -> io::Result<super::IntegrationStatus> {
    let spec = spec_for(target)?;
    integration_status_at_with_paths(target, installed_path(paths, spec)?, paths)
}

/// Whether `target`'s agent is present on this host: its own config
/// directory already exists. Install never creates that directory, only
/// shepr's files and subdirectories inside it. Pi and OMP resolve to the
/// `extensions` directory inside the agent directory, which install creates
/// when missing, so for them the agent directory is its parent.
pub(crate) fn agent_present(paths: &AgentIntegrationPaths, target: Target) -> io::Result<bool> {
    let spec = spec_for(target)?;
    let directory = paths.directory(spec.directory)?;
    let agent_directory = match spec.directory {
        DirectoryKey::PiExtension | DirectoryKey::OmpExtension => {
            directory.parent().map(Path::to_path_buf).ok_or_else(|| {
                io::Error::other(format!(
                    "{} extension directory {} has no parent",
                    target.label(),
                    directory.display()
                ))
            })?
        }
        _ => directory,
    };
    super::file_ops::is_dir(&agent_directory)
}

/// Whether the Shepr-owned Grok hook config exactly matches the installed
/// integration. JSON formatting and object key order do not affect validity.
fn grok_hook_config_is_valid(hook_path: &Path) -> io::Result<bool> {
    let Some(hooks_dir) = hook_path.parent() else {
        return Ok(false);
    };
    let expected_config = super::targets::grok_hook_config(hook_path)?;
    let config_path = hooks_dir.join(super::GROK_HOOK_CONFIG_NAME);
    let Some(content) = read_config_content(&config_path)? else {
        return Ok(false);
    };
    let config = serde_json::from_str::<serde_json::Value>(&content).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot parse {}: {error}", config_path.display()),
        )
    })?;
    Ok(config == expected_config)
}

fn opencode_tui_integration_is_valid(plugin_path: &Path, state_dir: &Path) -> io::Result<bool> {
    let Some(config_dir) = plugin_path.parent().and_then(Path::parent) else {
        return Ok(false);
    };
    let tui_plugin_path = config_dir.join(super::OPENCODE_TUI_PLUGIN_INSTALL_NAME);
    let tui_plugin_current =
        file_matches_asset(&tui_plugin_path, super::OPENCODE_TUI_PLUGIN_ASSET)?;
    let v2_plugin_path = config_dir
        .join(super::OPENCODE_V2_TUI_PLUGIN_DIR)
        .join("tui.js");
    let v2_plugin_current =
        file_matches_asset(&v2_plugin_path, super::OPENCODE_V2_TUI_PLUGIN_ASSET)?;
    if !tui_plugin_current || !v2_plugin_current {
        return Ok(false);
    }
    if !super::opencode_config::tui_plugin_is_configured(
        config_dir,
        super::OPENCODE_TUI_PLUGIN_SPEC,
    )? {
        return Ok(false);
    }
    super::opencode_config::cli_plugin_is_registered_or_deferred(
        config_dir,
        state_dir,
        super::OPENCODE_V2_TUI_PLUGIN_SPEC,
    )
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
    Nested {
        matcher: Option<String>,
        timeout_seconds: u64,
    },
    Flat {
        timeout_millis: u64,
    },
    Direct {
        timeout_seconds: u64,
    },
    Simple,
}

/// Whether `entries` holds `command` in the shape its installer writes, under
/// the installer's matcher. Install first strips every entry invoking shepr's
/// hook path from every event, whatever its matcher or extra fields, and then
/// writes the canonical set, so anything this rejects a reinstall repairs.
///
/// There is deliberately no per-entry `disabled` or `enabled` check: none of
/// these agents documents such a field (Claude Code only offers the global
/// `disableAllHooks`, Cursor has neither), so an entry carrying one still runs
/// and still counts as registered.
fn json_event_has_command(
    entries: &serde_json::Value,
    command: &str,
    shape: &JsonHookShape,
) -> bool {
    let Some(entries) = entries.as_array() else {
        return false;
    };
    match shape {
        JsonHookShape::Nested {
            matcher,
            timeout_seconds,
        } => entries.iter().any(|group| {
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
                        hooks.iter().any(|hook| {
                            is_matching_command_hook(hook, command)
                                && hook.get("timeout").and_then(serde_json::Value::as_u64)
                                    == Some(*timeout_seconds)
                        })
                    })
        }),
        JsonHookShape::Flat { timeout_millis } => entries.iter().any(|hook| {
            hook.get("matcher").is_none()
                && is_matching_command_hook(hook, command)
                && hook.get("timeout").and_then(serde_json::Value::as_u64) == Some(*timeout_millis)
                && hook.get("description").and_then(serde_json::Value::as_str)
                    == Some(super::config_edit::MASTRACODE_HOOK_DESCRIPTION)
        }),
        JsonHookShape::Direct { timeout_seconds } => entries.iter().any(|hook| {
            hook.get("matcher").is_none()
                && hook.get("type").and_then(serde_json::Value::as_str) == Some("command")
                && hook
                    .get(direct_command_field())
                    .and_then(serde_json::Value::as_str)
                    == Some(command)
                && hook.get("timeoutSec").and_then(serde_json::Value::as_u64)
                    == Some(*timeout_seconds)
        }),
        JsonHookShape::Simple => entries.iter().any(|hook| {
            hook.get("matcher").is_none()
                && hook.get("command").and_then(serde_json::Value::as_str) == Some(command)
        }),
    }
}

fn read_config_content(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", path.display()),
        )),
    }
}

fn read_json(path: &Path) -> io::Result<Option<serde_json::Value>> {
    let Some(content) = read_config_content(path)? else {
        return Ok(None);
    };
    serde_json::from_str(&content).map(Some).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot parse {}: {error}", path.display()),
        )
    })
}

/// Expected hook commands use the installer's shape, and no other command
/// invokes this hook path under a different event or action.
fn json_hook_commands_registered(
    config_path: &Path,
    root: HooksRoot,
    expected: &[(&str, String)],
    shape: &JsonHookShape,
    hook_path: &Path,
) -> io::Result<bool> {
    let Some(document) = read_json(config_path)? else {
        return Ok(false);
    };
    let events = match root {
        HooksRoot::HooksKey => document.get("hooks"),
        HooksRoot::Document => Some(&document),
    };
    let Some(events) = events.and_then(serde_json::Value::as_object) else {
        return Ok(false);
    };
    let expected_are_canonical = expected.iter().all(|(event, command)| {
        events
            .get(*event)
            .is_some_and(|entries| json_event_has_command(entries, command, shape))
    });
    let mut installed = Vec::new();
    for (event, entries) in events {
        collect_hook_path_commands(entries, hook_path, event, &mut installed);
    }
    let mut expected_commands = expected
        .iter()
        .map(|(event, command)| ((*event).to_string(), command.clone()))
        .collect::<Vec<_>>();
    installed.sort();
    expected_commands.sort();
    Ok(expected_are_canonical && installed == expected_commands)
}

fn collect_hook_path_commands(
    value: &serde_json::Value,
    hook_path: &Path,
    event: &str,
    output: &mut Vec<(String, String)>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_hook_path_commands(value, hook_path, event, output);
            }
        }
        serde_json::Value::Object(object) => {
            for field in ["command", "bash"] {
                if let Some(command) = object.get(field).and_then(serde_json::Value::as_str)
                    && is_hook_command_for_path(command, hook_path)
                {
                    output.push((event.to_string(), command.to_string()));
                }
            }
            if let Some(hooks) = object.get("hooks") {
                collect_hook_path_commands(hooks, hook_path, event, output);
            }
        }
        _ => {}
    }
}

fn read_toml(path: &Path) -> io::Result<Option<toml::Value>> {
    let Some(content) = read_config_content(path)? else {
        return Ok(None);
    };
    toml::from_str(&content).map(Some).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot parse {}: {error}", path.display()),
        )
    })
}

fn codex_hooks_feature_enabled(config_path: &Path) -> io::Result<bool> {
    let feature_enabled = read_toml(config_path)?.and_then(|config| {
        config
            .get("features")
            .and_then(|features| features.get("hooks"))
            .and_then(toml::Value::as_bool)
    });
    Ok(feature_enabled == Some(true))
}

fn kimi_hooks_registered(config_path: &Path, hook_path: &Path) -> io::Result<bool> {
    let Some(content) = read_config_content(config_path)? else {
        return Ok(false);
    };
    let _config = toml::from_str::<toml::Value>(&content).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot parse {}: {error}", config_path.display()),
        )
    })?;
    super::config_edit::kimi_config_block_is_current(&content, hook_path)
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
/// way install wrote it. The hook file alone cannot tell: an
/// install whose config edit failed, or a user who deleted the settings entry,
/// leaves a current hook script the agent never runs.
///
/// The config files are read from the directory the spec row's `path` is
/// installed under, so the depth follows the row instead of a hand-kept count.
fn hook_registration_is_current(
    spec: &IntegrationSpec,
    hook_path: &Path,
    paths: &AgentIntegrationPaths,
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
        RegistrationCheck::Grok => grok_hook_config_is_valid(hook_path)?,
        RegistrationCheck::Opencode => {
            return opencode_tui_integration_is_valid(
                hook_path,
                &paths.directory(DirectoryKey::OpencodeState)?,
            );
        }
        RegistrationCheck::Kimi => kimi_hooks_registered(&config(0)?, hook_path)?,
        RegistrationCheck::AntigravityCli => {
            let expected_block = super::targets::antigravity_cli_hook_block(hook_path)?;
            read_json(&config(0)?)?.is_some_and(|document| {
                document.get(super::ANTIGRAVITY_CLI_HOOK_BLOCK_NAME) == Some(&expected_block)
            })
        }
        RegistrationCheck::Codex => {
            let hook_commands_registered = json_hook_commands_registered(
                &config(0)?,
                HooksRoot::HooksKey,
                &hook_event_commands(hook_path, spec.target.hook_events()),
                &JsonHookShape::Nested {
                    matcher: None,
                    timeout_seconds: hook_timeout_seconds(spec)?,
                },
                hook_path,
            )?;
            let hooks_feature_enabled = codex_hooks_feature_enabled(&config(1)?)?;
            hook_commands_registered && hooks_feature_enabled
        }
        RegistrationCheck::Json { root, shape } => {
            let expected = match shape {
                // A direct entry is written for every event, including the
                // ones whose hook takes no action argument.
                JsonShape::Direct => spec
                    .target
                    .hook_events()
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
                _ => hook_event_commands(hook_path, spec.target.hook_events()),
            };
            let shape = match shape {
                JsonShape::Nested => JsonHookShape::Nested {
                    matcher: None,
                    timeout_seconds: hook_timeout_seconds(spec)?,
                },
                JsonShape::NestedClaude => JsonHookShape::Nested {
                    matcher: Some(super::claude_settings::claude_session_start_matcher()),
                    timeout_seconds: hook_timeout_seconds(spec)?,
                },
                JsonShape::Flat => JsonHookShape::Flat {
                    timeout_millis: hook_timeout_millis(spec)?,
                },
                JsonShape::Direct => JsonHookShape::Direct {
                    timeout_seconds: hook_timeout_seconds(spec)?,
                },
                JsonShape::Simple => JsonHookShape::Simple,
            };
            json_hook_commands_registered(&config(0)?, root, &expected, &shape, hook_path)?
        }
    };
    Ok(registered)
}

fn hook_timeout(spec: &IntegrationSpec) -> io::Result<Duration> {
    spec.hook_timeout.ok_or_else(|| {
        io::Error::other(format!(
            "integration spec for {:?} has no hook timeout",
            spec.target
        ))
    })
}

fn hook_timeout_seconds(spec: &IntegrationSpec) -> io::Result<u64> {
    Ok(hook_timeout(spec)?.as_secs())
}

fn hook_timeout_millis(spec: &IntegrationSpec) -> io::Result<u64> {
    u64::try_from(hook_timeout(spec)?.as_millis())
        .map_err(|_| io::Error::other("hook timeout exceeds millisecond configuration range"))
}

fn file_matches_asset(path: &Path, asset: &str) -> io::Result<bool> {
    let installed = super::file_ops::is_file(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot stat {}: {error}", path.display()),
        )
    })?;
    if !installed {
        return Ok(false);
    }
    let content = fs::read(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", path.display()),
        )
    })?;
    Ok(content.as_slice() == asset.as_bytes())
}

fn integration_state_for_path(
    path: &Path,
    expected_asset: &str,
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

    let content = fs::read(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", path.display()),
        )
    })?;
    let installed_version = std::str::from_utf8(&content)
        .ok()
        .and_then(parse_integration_version);
    // Only release launches install these shared artifacts. Exact bytes detect
    // edits without trusting a larger version marker or requiring a manual bump.
    // Dev launches must skip status-driven installation altogether.
    let state = if content.as_slice() == expected_asset.as_bytes() {
        super::IntegrationStatusKind::Current
    } else {
        super::IntegrationStatusKind::Outdated
    };

    Ok((state, installed_version))
}

/// The status of the integration installed at `path`. A stat or read error on
/// its asset, or a stat, read or parse error on its registration config, is
/// returned. A missing registration reads `Outdated`, so the next install
/// repairs it.
fn integration_status_at_with_paths(
    target: crate::agent::IntegrationTarget,
    path: PathBuf,
    paths: &AgentIntegrationPaths,
) -> io::Result<super::IntegrationStatus> {
    let spec = spec_for(target)?;
    let expected_asset =
        spec.assets.first().copied().ok_or_else(|| {
            io::Error::other(format!("integration spec for {target:?} has no asset"))
        })?;
    let (mut state, installed_version) = integration_state_for_path(&path, expected_asset)?;

    if state == super::IntegrationStatusKind::Current
        && !hook_registration_is_current(spec, &path, paths)?
    {
        state = super::IntegrationStatusKind::Outdated;
    }

    Ok(super::IntegrationStatus {
        target,
        path,
        state,
        installed_version,
    })
}

/// Parses the optional marker for logs. It does not determine whether an
/// installed integration is current.
fn parse_integration_version(content: &str) -> Option<u32> {
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

/// `integration_status_at_with_paths` with the paths resolved from the
/// process environment.
#[cfg(test)]
pub(crate) fn integration_status_at(
    target: crate::agent::IntegrationTarget,
    path: PathBuf,
) -> io::Result<super::IntegrationStatus> {
    let paths = AgentIntegrationPaths::resolve();
    integration_status_at_with_paths(target, path, &paths)
}

/// One status per supported target, in spec order.
#[cfg(test)]
pub(crate) fn integration_status_rows(
    paths: &AgentIntegrationPaths,
) -> Vec<io::Result<super::IntegrationStatus>> {
    INTEGRATION_SPECS
        .iter()
        .map(|spec| integration_status(paths, spec.target))
        .collect()
}

#[cfg(test)]
mod registration_tests {
    use super::*;
    use crate::agent::IntegrationTarget;
    use crate::integration::IntegrationStatusKind;

    #[test]
    fn antigravity_integration_uses_the_canonical_agent_label() {
        assert_eq!(
            IntegrationTarget::AntigravityCli.label(),
            crate::agent::Agent::Antigravity.label()
        );
    }

    /// Status compares the installed files with the bundled bytes and the
    /// agent config with what install writes, so every target must read
    /// Current straight after its own install.
    #[test]
    fn every_target_reads_current_right_after_install() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = AgentIntegrationPaths::resolve();
        for spec in INTEGRATION_SPECS {
            let label = spec.target.label();
            let directory = paths.directory(spec.directory).expect("test precondition");
            let agent_directory = match spec.directory {
                DirectoryKey::PiExtension | DirectoryKey::OmpExtension => directory
                    .parent()
                    .map(Path::to_path_buf)
                    .expect("test precondition"),
                _ => directory,
            };
            fs::create_dir_all(&agent_directory).expect("test precondition");
            install_operation(&paths, spec.target)
                .unwrap_or_else(|error| panic!("{label} install failed: {error}"));
            let status = integration_status(&paths, spec.target)
                .unwrap_or_else(|error| panic!("{label} status failed: {error}"));
            assert_eq!(status.state, IntegrationStatusKind::Current, "{label}");
        }
    }

    #[test]
    fn bundled_integration_specs_register_their_assets() {
        for spec in INTEGRATION_SPECS {
            assert!(
                !spec.assets.is_empty(),
                "{} must register its bundled assets",
                spec.target.label()
            );
            for (index, asset) in spec.assets.iter().enumerate() {
                assert!(
                    parse_integration_version(asset).is_some(),
                    "{} bundled asset {index} must carry diagnostic version metadata",
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
        // An unresolved directory is an error for the presence check too, so
        // auto-install logs it instead of reading the agent as absent.
        assert!(
            IntegrationTarget::all().any(|target| agent_present(&paths, target).is_err()),
            "presence under HOME cannot resolve without it"
        );
    }

    #[test]
    fn config_status_readers_distinguish_missing_files_from_errors() {
        let dir = base("config-status-readers");
        let json_path = dir.join("settings.json");
        let toml_path = dir.join("config.toml");

        assert!(
            read_json(&json_path)
                .expect("missing JSON config")
                .is_none()
        );
        assert!(
            read_toml(&toml_path)
                .expect("missing TOML config")
                .is_none()
        );

        fs::write(&json_path, "{ invalid json").expect("test precondition");
        let error = read_json(&json_path).expect_err("invalid JSON must be reported");
        assert!(error.to_string().contains("cannot parse"));

        fs::write(&toml_path, "[broken\n").expect("test precondition");
        let error = read_toml(&toml_path).expect_err("invalid TOML must be reported");
        assert!(error.to_string().contains("cannot parse"));

        fs::remove_file(&json_path).expect("test precondition");
        fs::create_dir(&json_path).expect("test precondition");
        let error = read_json(&json_path).expect_err("JSON read failure must be reported");
        assert!(error.to_string().contains("cannot read"));
    }

    fn base(name: &str) -> PathBuf {
        shepr_test_support::ScratchDir::new(name).to_path_buf()
    }

    fn write_current_hook(target: IntegrationTarget, path: &Path) {
        fs::create_dir_all(path.parent().expect("test precondition")).expect("test precondition");
        let asset = integration_asset(target).expect("integration asset");
        fs::write(path, asset).expect("test precondition");
    }

    fn state(target: IntegrationTarget, hook: &Path) -> IntegrationStatusKind {
        integration_status_at(target, hook.to_path_buf())
            .expect("stat hook")
            .state
    }

    #[test]
    fn claude_hook_without_settings_entry_is_outdated() {
        let dir = base("claude");
        let hook = dir.join("hooks").join("shepr-agent-state.sh");
        write_current_hook(IntegrationTarget::Claude, &hook);
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
        let error = integration_status_at(IntegrationTarget::Claude, hook.clone())
            .expect_err("invalid config must be reported");
        assert!(error.to_string().contains("cannot parse"));

        fs::remove_file(&settings_path).expect("test precondition");
        fs::create_dir(&settings_path).expect("test precondition");
        let error = integration_status_at(IntegrationTarget::Claude, hook)
            .expect_err("config read failure must be reported");
        assert!(error.to_string().contains("cannot read"));
    }

    #[test]
    fn claude_command_outside_the_installed_shape_is_outdated() {
        let dir = base("claude-shape");
        let hook = dir.join("hooks").join("shepr-agent-state.sh");
        write_current_hook(IntegrationTarget::Claude, &hook);
        let settings_path = dir.join("settings.json");
        let command = hook_command(&hook, Some("session"));
        let matcher = super::super::claude_settings::claude_session_start_matcher();
        let write = |session_start: serde_json::Value| {
            let settings = serde_json::json!({ "hooks": { "SessionStart": session_start } });
            fs::write(&settings_path, settings.to_string()).expect("test precondition");
        };

        write(serde_json::json!([
            { "matcher": matcher, "hooks": [{ "type": "command", "command": command, "timeout": crate::limits::HOOK_TIMEOUT.as_secs() }] }
        ]));
        assert_eq!(
            state(IntegrationTarget::Claude, &hook),
            IntegrationStatusKind::Current
        );

        // Claude has no per-hook disable switch, so an unknown `disabled` field
        // does not stop the hook running and does not unregister it.
        write(serde_json::json!([{ "matcher": matcher, "hooks": [
            { "type": "command", "command": command, "timeout": crate::limits::HOOK_TIMEOUT.as_secs(), "disabled": true }
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
        write_current_hook(IntegrationTarget::Codex, &hook);
        let entry = |action| {
            serde_json::json!([
                { "hooks": [{ "type": "command", "command": hook_command(&hook, Some(action)), "timeout": 10 }] }
            ])
        };
        let hooks_json = serde_json::json!({
            "hooks": {
                "SessionStart": entry("session"),
                "UserPromptSubmit": entry("working"),
                "Stop": entry("idle"),
                "Interrupt": entry("idle"),
            }
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
        write_current_hook(IntegrationTarget::Kimi, &hook);
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
    fn kimi_status_rejects_and_install_removes_stale_managed_event() {
        let env = shepr_test_support::IsolatedEnv::new();
        let dir = env.home().join(".kimi-code");
        fs::create_dir_all(&dir).expect("test precondition");
        let paths = AgentIntegrationPaths::resolve();
        super::super::targets::install_kimi(&paths).expect("install");

        let status = integration_status(&paths, IntegrationTarget::Kimi).expect("status");
        assert_eq!(status.state, IntegrationStatusKind::Current);
        let hook = status.path;
        let config_path = dir.join(super::super::KIMI_CONFIG_NAME);
        let config = fs::read_to_string(&config_path).expect("test precondition");
        let stale_registration = format!(
            "[[hooks]]\nevent = \"OldEvent\"\ncommand = {}\ntimeout = 10\n\n{}",
            super::super::config_edit::toml_basic_string(&hook_command(&hook, Some("old-action"),)),
            super::super::KIMI_CONFIG_BLOCK_END,
        );
        let stale = config.replace(super::super::KIMI_CONFIG_BLOCK_END, &stale_registration);
        assert_ne!(stale, config, "test precondition");
        fs::write(&config_path, stale).expect("test precondition");
        assert_eq!(
            integration_status(&paths, IntegrationTarget::Kimi)
                .expect("status")
                .state,
            IntegrationStatusKind::Outdated
        );

        super::super::targets::install_kimi(&paths).expect("reinstall");
        assert_eq!(
            integration_status(&paths, IntegrationTarget::Kimi)
                .expect("status")
                .state,
            IntegrationStatusKind::Current
        );
        assert!(
            !fs::read_to_string(&config_path)
                .expect("test precondition")
                .contains("OldEvent")
        );
    }

    #[test]
    fn mastracode_checks_flat_top_level_events() {
        let dir = base("mastracode");
        let hook = dir.join("hooks").join("shepr-agent-state.sh");
        write_current_hook(IntegrationTarget::Mastracode, &hook);
        let mut document = serde_json::Map::new();
        let timeout_millis = hook_timeout_millis(
            spec_for(IntegrationTarget::Mastracode).expect("test precondition"),
        )
        .expect("test precondition");
        for event_spec in integration_hook_events(IntegrationTarget::Mastracode) {
            let Some(action) = event_spec.action else {
                continue;
            };
            document.insert(
                event_spec.event.to_string(),
                serde_json::json!([{ "type": "command", "command": hook_command(&hook, Some(action.as_str())), "timeout": timeout_millis, "description": super::super::config_edit::MASTRACODE_HOOK_DESCRIPTION }]),
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
        let cases: [(IntegrationTarget, &[&str], &str, HooksRoot, Install); 7] = [
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
                let mut document = read_json(&config_path)
                    .expect("read config")
                    .expect("config exists");
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

            let hook_path = status().path;
            let stale_command = super::super::command::hook_command(&hook_path, Some("old-action"));
            let stale_entry = match target {
                IntegrationTarget::Claude
                | IntegrationTarget::Codex
                | IntegrationTarget::Devin
                | IntegrationTarget::Droid => serde_json::json!({
                    "matcher": "previous matcher",
                    "hooks": [{
                        "type": "command",
                        "command": stale_command,
                        "timeout": 5,
                    }],
                }),
                IntegrationTarget::Copilot => serde_json::json!({
                    "type": "command",
                    "bash": stale_command,
                    "timeoutSec": 5,
                }),
                IntegrationTarget::Cursor => serde_json::json!({"command": stale_command}),
                IntegrationTarget::Mastracode => serde_json::json!({
                    "type": "command",
                    "command": stale_command,
                    "timeout": 5,
                    "description": super::super::config_edit::MASTRACODE_HOOK_DESCRIPTION,
                }),
                _ => unreachable!("only JSON hook targets are listed"),
            };
            let mut document = read_json(&config_path)
                .expect("read config")
                .expect("config exists");
            let events = match root {
                HooksRoot::HooksKey => document.get_mut("hooks"),
                HooksRoot::Document => Some(&mut document),
            }
            .and_then(serde_json::Value::as_object_mut)
            .expect("test precondition");
            events
                .entry("PreviousEvent".to_owned())
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .expect("test precondition")
                .push(stale_entry);
            fs::write(&config_path, document.to_string()).expect("test precondition");
            assert_eq!(
                status().state,
                IntegrationStatusKind::Outdated,
                "{target:?}"
            );

            install(&paths).expect("reinstall removes stale event registrations");
            assert_eq!(status().state, IntegrationStatusKind::Current, "{target:?}");
            let document = read_json(&config_path)
                .expect("read config")
                .expect("config exists");
            let events = match root {
                HooksRoot::HooksKey => document.get("hooks"),
                HooksRoot::Document => Some(&document),
            }
            .and_then(serde_json::Value::as_object)
            .expect("test precondition");
            assert!(!events.contains_key("PreviousEvent"), "{target:?}");
        }
    }

    #[test]
    fn every_target_has_exactly_one_spec() {
        for target in IntegrationTarget::all() {
            assert_eq!(
                INTEGRATION_SPECS
                    .iter()
                    .filter(|spec| spec.target == target)
                    .count(),
                1,
                "{target:?}"
            );
        }
        assert_eq!(INTEGRATION_SPECS.len(), IntegrationTarget::all().count());
    }

    #[test]
    fn agent_presence_is_the_agent_config_directory() {
        let env = shepr_test_support::IsolatedEnv::new();
        let home = env.home();
        let paths = super::super::env::AgentIntegrationPaths::resolve();
        for target in IntegrationTarget::all() {
            assert!(
                !agent_present(&paths, target).expect("presence resolves"),
                "{target:?} is absent in a fresh home"
            );
        }

        // Pi's agent directory exists but its extensions directory does not:
        // the agent is present, and install creates `extensions`.
        fs::create_dir_all(home.join(".pi").join("agent")).expect("test precondition");
        fs::create_dir_all(home.join(".claude")).expect("test precondition");
        let paths = super::super::env::AgentIntegrationPaths::resolve();
        assert!(agent_present(&paths, IntegrationTarget::Pi).expect("presence resolves"));
        assert!(agent_present(&paths, IntegrationTarget::Claude).expect("presence resolves"));
        assert!(!agent_present(&paths, IntegrationTarget::Codex).expect("presence resolves"));
    }

    #[test]
    fn plugin_directory_targets_need_only_the_file() {
        let dir = base("pi");
        let plugin = dir.join("extensions").join("shepr-agent-state.ts");
        write_current_hook(IntegrationTarget::Pi, &plugin);
        assert_eq!(
            state(IntegrationTarget::Pi, &plugin),
            IntegrationStatusKind::Current
        );
    }
}
