use crate::types::InstallResult;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shepr_agent::IntegrationTarget as Target;

use super::config_edit::HOOK_COMMAND_FIELDS;
use super::env::{AgentIntegrationPaths, IntegrationEnvironment};
use super::registration::{HookEventPolicy, HooksRoot, JsonShape, Registration, RequiredJsonField};
use super::types::{ArtifactRole, InstallError, IntegrationOutdatedReason};

#[derive(Clone, Copy)]
pub(super) struct ManagedAsset {
    pub(super) contents: &'static str,
    pub(super) path: &'static [&'static str],
    pub(super) executable: bool,
    pub(super) role: Option<ArtifactRole>,
}

#[derive(Clone, Copy)]
struct IntegrationSpec {
    target: Target,
    directory: fn(&IntegrationEnvironment) -> InstallResult<PathBuf>,
    primary_asset: ManagedAsset,
    additional_assets: &'static [ManagedAsset],
    action_label: Option<&'static str>,
    presence_directory: PresenceDirectory,
    different_directory_from: Option<Target>,
    registration: Registration,
}

#[derive(Clone, Copy)]
enum PresenceDirectory {
    TargetDirectory,
    ParentDirectory,
}

const NO_REQUIRED_JSON_FIELDS: &[RequiredJsonField] = &[];
const CURSOR_REQUIRED_JSON_FIELDS: &[RequiredJsonField] = &[RequiredJsonField {
    key: "version",
    default_number: 1,
}];

const fn spec_for(target: Target) -> &'static IntegrationSpec {
    match target {
        Target::Pi => &IntegrationSpec {
            target: Target::Pi,
            directory: super::env::pi_extension_dir,
            registration: Registration::DirectoryLoaded,
            primary_asset: ManagedAsset {
                contents: super::PI_EXTENSION_ASSET,
                path: &[super::PI_EXTENSION_INSTALL_NAME],
                executable: false,
                role: Some(ArtifactRole::Extension),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::ParentDirectory,
            different_directory_from: None,
        },
        Target::Omp => &IntegrationSpec {
            target: Target::Omp,
            directory: super::env::omp_extension_dir,
            registration: Registration::DirectoryLoaded,
            primary_asset: ManagedAsset {
                contents: super::OMP_EXTENSION_ASSET,
                path: &[super::OMP_EXTENSION_INSTALL_NAME],
                executable: false,
                role: Some(ArtifactRole::Extension),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::ParentDirectory,
            different_directory_from: Some(Target::Pi),
        },
        Target::Claude => &IntegrationSpec {
            target: Target::Claude,
            directory: super::env::claude_dir,
            registration: Registration::Json {
                file: super::CLAUDE_SETTINGS_NAME,
                root: HooksRoot::HooksKey,
                shape: JsonShape::Nested(super::HOOK_TIMEOUT),
                artifact_role: ArtifactRole::Settings,
                document_description: "Claude settings",
                required_fields: NO_REQUIRED_JSON_FIELDS,
                event_policy: HookEventPolicy::CLAUDE,
            },
            primary_asset: ManagedAsset {
                contents: super::CLAUDE_HOOK_ASSET,
                path: &["hooks", super::CLAUDE_HOOK_INSTALL_NAME],
                executable: true,
                role: Some(ArtifactRole::Hook),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::Codex => &IntegrationSpec {
            target: Target::Codex,
            directory: super::env::codex_dir,
            registration: Registration::Codex {
                hooks: super::CODEX_HOOKS_NAME,
                config: super::CODEX_CONFIG_NAME,
                timeout: super::HOOK_TIMEOUT,
                hooks_artifact_role: ArtifactRole::Hooks,
                config_artifact_role: ArtifactRole::Config,
                document_description: "Codex hooks file",
                required_fields: NO_REQUIRED_JSON_FIELDS,
                event_policy: HookEventPolicy::DESCRIPTOR,
            },
            primary_asset: ManagedAsset {
                contents: super::CODEX_HOOK_ASSET,
                path: &[super::CODEX_HOOK_INSTALL_NAME],
                executable: true,
                role: Some(ArtifactRole::Hook),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::Copilot => &IntegrationSpec {
            target: Target::Copilot,
            directory: super::env::copilot_dir,
            registration: Registration::Json {
                file: super::COPILOT_SETTINGS_NAME,
                root: HooksRoot::HooksKey,
                shape: JsonShape::Direct(super::HOOK_TIMEOUT),
                artifact_role: ArtifactRole::Settings,
                document_description: "Copilot settings",
                required_fields: NO_REQUIRED_JSON_FIELDS,
                event_policy: HookEventPolicy::COPILOT,
            },
            primary_asset: ManagedAsset {
                contents: super::COPILOT_HOOK_ASSET,
                path: &["hooks", super::COPILOT_HOOK_INSTALL_NAME],
                executable: true,
                role: Some(ArtifactRole::Hook),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::Devin => &IntegrationSpec {
            target: Target::Devin,
            directory: super::env::devin_dir,
            registration: Registration::Json {
                file: super::DEVIN_CONFIG_NAME,
                root: HooksRoot::HooksKey,
                shape: JsonShape::Nested(super::HOOK_TIMEOUT),
                artifact_role: ArtifactRole::Settings,
                document_description: "Devin config",
                required_fields: NO_REQUIRED_JSON_FIELDS,
                event_policy: HookEventPolicy::DESCRIPTOR,
            },
            primary_asset: ManagedAsset {
                contents: super::DEVIN_HOOK_ASSET,
                path: &[super::DEVIN_HOOK_INSTALL_NAME],
                executable: true,
                role: Some(ArtifactRole::Hook),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::Droid => &IntegrationSpec {
            target: Target::Droid,
            directory: super::env::droid_dir,
            registration: Registration::Json {
                file: super::DROID_SETTINGS_NAME,
                root: HooksRoot::HooksKey,
                shape: JsonShape::Nested(super::HOOK_TIMEOUT),
                artifact_role: ArtifactRole::Hooks,
                document_description: "Droid settings",
                required_fields: NO_REQUIRED_JSON_FIELDS,
                event_policy: HookEventPolicy::DESCRIPTOR,
            },
            primary_asset: ManagedAsset {
                contents: super::DROID_HOOK_ASSET,
                path: &["hooks", super::DROID_HOOK_INSTALL_NAME],
                executable: true,
                role: Some(ArtifactRole::Hook),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::Kimi => &IntegrationSpec {
            target: Target::Kimi,
            directory: super::env::kimi_dir,
            registration: Registration::Kimi {
                file: super::KIMI_CONFIG_NAME,
                timeout: super::HOOK_TIMEOUT,
            },
            primary_asset: ManagedAsset {
                contents: super::KIMI_HOOK_ASSET,
                path: &["hooks", super::KIMI_HOOK_INSTALL_NAME],
                executable: true,
                role: Some(ArtifactRole::Hook),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::Opencode => &IntegrationSpec {
            target: Target::Opencode,
            directory: super::env::opencode_dir,
            registration: Registration::Opencode,
            primary_asset: ManagedAsset {
                contents: super::OPENCODE_PLUGIN_ASSET,
                path: &["plugins", super::OPENCODE_PLUGIN_INSTALL_NAME],
                executable: false,
                role: Some(ArtifactRole::Plugin),
            },
            additional_assets: &[
                ManagedAsset {
                    contents: super::OPENCODE_TUI_PLUGIN_ASSET,
                    path: &[super::OPENCODE_TUI_PLUGIN_INSTALL_NAME],
                    executable: false,
                    role: Some(ArtifactRole::TuiPlugin),
                },
                ManagedAsset {
                    contents: super::OPENCODE_V2_TUI_PLUGIN_ASSET,
                    path: &[super::OPENCODE_V2_TUI_PLUGIN_DIR, "tui.js"],
                    executable: false,
                    role: None,
                },
            ],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::Kilo => &IntegrationSpec {
            target: Target::Kilo,
            directory: super::env::kilo_dir,
            registration: Registration::DirectoryLoaded,
            primary_asset: ManagedAsset {
                contents: super::KILO_PLUGIN_ASSET,
                path: &["plugin", super::KILO_PLUGIN_INSTALL_NAME],
                executable: false,
                role: Some(ArtifactRole::Plugin),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::Cursor => &IntegrationSpec {
            target: Target::Cursor,
            directory: super::env::cursor_dir,
            registration: Registration::Json {
                file: super::CURSOR_HOOKS_NAME,
                root: HooksRoot::HooksKey,
                shape: JsonShape::Simple,
                artifact_role: ArtifactRole::UpdatedHooks,
                document_description: "Cursor hooks file",
                required_fields: CURSOR_REQUIRED_JSON_FIELDS,
                event_policy: HookEventPolicy::DESCRIPTOR,
            },
            primary_asset: ManagedAsset {
                contents: super::CURSOR_HOOK_ASSET,
                path: &[super::CURSOR_HOOK_INSTALL_NAME],
                executable: true,
                role: Some(ArtifactRole::Hook),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::Mastracode => &IntegrationSpec {
            target: Target::Mastracode,
            directory: super::env::mastracode_dir,
            registration: Registration::Json {
                file: super::MASTRACODE_HOOKS_NAME,
                root: HooksRoot::Document,
                shape: JsonShape::Flat(super::HOOK_TIMEOUT),
                artifact_role: ArtifactRole::Hooks,
                document_description: "MastraCode hooks file",
                required_fields: NO_REQUIRED_JSON_FIELDS,
                event_policy: HookEventPolicy::DESCRIPTOR,
            },
            primary_asset: ManagedAsset {
                contents: super::MASTRACODE_HOOK_ASSET,
                path: &["hooks", super::MASTRACODE_HOOK_INSTALL_NAME],
                executable: true,
                role: Some(ArtifactRole::Hook),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::AntigravityCli => &IntegrationSpec {
            target: Target::AntigravityCli,
            directory: super::env::antigravity_cli_dir,
            registration: Registration::AntigravityCli {
                file: super::ANTIGRAVITY_CLI_HOOKS_NAME,
                timeout: super::HOOK_TIMEOUT,
            },
            primary_asset: ManagedAsset {
                contents: super::ANTIGRAVITY_CLI_HOOK_ASSET,
                path: &["hooks", super::ANTIGRAVITY_CLI_HOOK_INSTALL_NAME],
                executable: true,
                role: Some(ArtifactRole::Hook),
            },
            additional_assets: &[],
            action_label: Some("antigravity-cli"),
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
        Target::Grok => &IntegrationSpec {
            target: Target::Grok,
            directory: super::env::grok_dir,
            registration: Registration::Grok {
                file: super::GROK_HOOK_CONFIG_NAME,
                timeout: super::HOOK_TIMEOUT,
            },
            primary_asset: ManagedAsset {
                contents: super::GROK_HOOK_ASSET,
                path: &["hooks", super::GROK_HOOK_INSTALL_NAME],
                executable: true,
                role: Some(ArtifactRole::Hook),
            },
            additional_assets: &[],
            action_label: None,
            presence_directory: PresenceDirectory::TargetDirectory,
            different_directory_from: None,
        },
    }
}

pub(super) fn resolve_target_directory(
    environment: &IntegrationEnvironment,
    target: Target,
) -> InstallResult<PathBuf> {
    (spec_for(target).directory)(environment)
}

pub(super) fn registration(target: Target) -> Registration {
    spec_for(target).registration
}

pub(super) fn target_directory(
    paths: &AgentIntegrationPaths,
    target: Target,
) -> InstallResult<PathBuf> {
    paths.directory(target)
}

pub(super) fn target_path(paths: &AgentIntegrationPaths, target: Target) -> InstallResult<PathBuf> {
    installed_path(paths, spec_for(target))
}

pub(crate) fn action_label(target: Target) -> &'static str {
    spec_for(target)
        .action_label
        .unwrap_or_else(|| target.label())
}

pub(super) fn directory_must_differ_from(target: Target) -> Option<Target> {
    spec_for(target).different_directory_from
}

pub(super) fn managed_assets(target: Target) -> impl Iterator<Item = &'static ManagedAsset> {
    let spec = spec_for(target);
    std::iter::once(&spec.primary_asset).chain(spec.additional_assets.iter())
}

/// The primary managed file `spec` installs, whose bundled bytes status checks.
fn installed_path(paths: &AgentIntegrationPaths, spec: &IntegrationSpec) -> InstallResult<PathBuf> {
    let mut path = paths.directory(spec.target)?;
    for part in spec.primary_asset.path {
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
) -> Result<super::IntegrationStatus, InstallError> {
    let spec = spec_for(target);
    let path = installed_path(paths, spec)?;
    integration_status_at_with_paths(target, path, paths)
}

/// Whether `target`'s agent is present on this host: its own config
/// directory already exists. Install never creates that directory, only
/// shepr's files and subdirectories inside it. Pi and OMP resolve to the
/// `extensions` directory inside the agent directory, which install creates
/// when missing, so for them the agent directory is its parent.
pub(crate) fn agent_present(paths: &AgentIntegrationPaths, target: Target) -> InstallResult<bool> {
    super::file_ops::is_dir(&agent_directory(paths, target)?)
}

pub(super) fn agent_directory(
    paths: &AgentIntegrationPaths,
    target: Target,
) -> InstallResult<PathBuf> {
    let spec = spec_for(target);
    let directory = paths.directory(target)?;
    let agent_directory = match spec.presence_directory {
        PresenceDirectory::TargetDirectory => directory,
        PresenceDirectory::ParentDirectory => {
            directory.parent().map(Path::to_path_buf).ok_or_else(|| {
                InstallError::from(io::Error::other(format!(
                    "{} extension directory {} has no parent",
                    target.label(),
                    directory.display()
                )))
            })?
        }
    };
    Ok(agent_directory)
}

/// Whether the Shepr-owned Grok hook config exactly matches the installed
/// integration. JSON formatting and object key order do not affect validity.
fn grok_hook_config_is_valid(
    config_path: &Path,
    hook_path: &Path,
    timeout: Duration,
) -> InstallResult<bool> {
    let expected_config = super::targets::grok_hook_config_with_timeout(hook_path, timeout)?;
    let Some(content) = read_config_content(config_path)? else {
        return Ok(false);
    };
    let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) else {
        // This file is wholly Shepr-owned, so malformed contents are drift
        // that install can safely replace rather than a user config error.
        return Ok(false);
    };
    Ok(config == expected_config)
}

fn opencode_tui_integration_is_valid(plugin_path: &Path, state_dir: &Path) -> InstallResult<bool> {
    let Some(config_dir) = plugin_path.parent().and_then(Path::parent) else {
        return Ok(false);
    };
    for asset in managed_assets(Target::Opencode).skip(1) {
        let mut path = config_dir.to_path_buf();
        for part in asset.path {
            path.push(part);
        }
        if !file_matches_asset(&path, asset.contents)? {
            return Ok(false);
        }
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

// Registration reads use the same regular-file policy as install.
fn read_config_content(path: &Path) -> InstallResult<Option<String>> {
    super::file_ops::read_if_file(path)
}

fn read_json(path: &Path) -> InstallResult<Option<serde_json::Value>> {
    let Some(content) = read_config_content(path)? else {
        return Ok(None);
    };
    serde_json::from_str(&content).map(Some).map_err(|error| {
        InstallError::config_unparseable(format!("cannot parse {}: {error}", path.display()))
    })
}

/// Check the same canonical entries installation merges, allowing unrelated
/// fields and hooks. A matcher absent from the canonical entry must stay absent.
fn canonical_entry_matches(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match expected {
        serde_json::Value::Object(fields) => actual.as_object().is_some_and(|object| {
            fields.iter().all(|(key, value)| {
                object
                    .get(key)
                    .is_some_and(|actual| canonical_entry_matches(actual, value))
            })
        }),
        serde_json::Value::Array(entries) => actual.as_array().is_some_and(|actual| {
            entries.iter().all(|entry| {
                actual
                    .iter()
                    .any(|value| canonical_entry_matches(value, entry))
            })
        }),
        _ => actual == expected,
    }
}

fn json_hook_commands_registered(
    config_path: &Path,
    root: HooksRoot,
    expected: &serde_json::Map<String, serde_json::Value>,
    required_fields: &[RequiredJsonField],
    hook_path: &Path,
) -> InstallResult<bool> {
    let Some(document) = read_json(config_path)? else {
        return Ok(false);
    };
    if !required_fields
        .iter()
        .all(|field| document.get(field.key).is_some())
    {
        return Ok(false);
    }
    let events = match root {
        HooksRoot::HooksKey => document.get("hooks"),
        HooksRoot::Document => Some(&document),
    };
    let Some(events) = events.and_then(serde_json::Value::as_object) else {
        return Ok(false);
    };
    let canonical = expected.iter().all(|(event, entries)| {
        let Some(actual) = events.get(event).and_then(serde_json::Value::as_array) else {
            return false;
        };
        entries.as_array().is_some_and(|entries| {
            entries.iter().all(|expected| {
                actual.iter().any(|entry| {
                    (expected.get("matcher").is_some() || entry.get("matcher").is_none())
                        && canonical_entry_matches(entry, expected)
                })
            })
        })
    });
    let mut installed = Vec::new();
    let mut commands = Vec::new();
    for (event, entries) in events {
        let expected_commands = expected
            .get(event)
            .map_or_else(Default::default, super::json_edit::expected_hook_commands);
        collect_hook_path_commands(
            entries,
            hook_path,
            event,
            &expected_commands,
            &mut installed,
        );
    }
    for (event, entries) in expected {
        let expected_commands = super::json_edit::expected_hook_commands(entries);
        collect_hook_path_commands(entries, hook_path, event, &expected_commands, &mut commands);
    }
    installed.sort();
    commands.sort();
    Ok(canonical && installed == commands)
}

fn collect_hook_path_commands(
    value: &serde_json::Value,
    hook_path: &Path,
    event: &str,
    expected_commands: &[String],
    output: &mut Vec<(String, String)>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_hook_path_commands(value, hook_path, event, expected_commands, output);
            }
        }
        serde_json::Value::Object(object) => {
            for &field in HOOK_COMMAND_FIELDS {
                if let Some(command) = object.get(field).and_then(serde_json::Value::as_str)
                    && super::json_edit::is_managed_hook_command(
                        command,
                        hook_path,
                        expected_commands,
                    )
                {
                    output.push((event.to_string(), command.to_string()));
                }
            }
            if let Some(hooks) = object.get("hooks") {
                collect_hook_path_commands(hooks, hook_path, event, expected_commands, output);
            }
        }
        _ => {}
    }
}

fn read_toml(path: &Path) -> InstallResult<Option<toml::Value>> {
    let Some(content) = read_config_content(path)? else {
        return Ok(None);
    };
    toml::from_str(&content).map(Some).map_err(|error| {
        InstallError::config_unparseable(format!("cannot parse {}: {error}", path.display()))
    })
}

fn codex_hooks_feature_enabled(config_path: &Path) -> InstallResult<bool> {
    let feature_enabled = read_toml(config_path)?.and_then(|config| {
        config
            .get("features")
            .and_then(|features| features.get("hooks"))
            .and_then(toml::Value::as_bool)
    });
    Ok(feature_enabled == Some(true))
}

fn kimi_hooks_registered(
    config_path: &Path,
    hook_path: &Path,
    timeout: Duration,
) -> InstallResult<bool> {
    let Some(content) = read_config_content(config_path)? else {
        return Ok(false);
    };
    // The registration comparison preserves TOML source text. Parse the full
    // file here so syntax errors inside the managed block are surfaced too.
    let _config = toml::from_str::<toml::Value>(&content).map_err(|error| {
        InstallError::config_unparseable(format!("cannot parse {}: {error}", config_path.display()))
    })?;
    super::config_edit::kimi_config_block_with_timeout_is_current(&content, hook_path, timeout)
}

fn hook_registration_is_current(
    spec: &IntegrationSpec,
    hook_path: &Path,
    paths: &AgentIntegrationPaths,
) -> InstallResult<bool> {
    let Some(dir) = ancestor(hook_path, spec.primary_asset.path.len()) else {
        return Ok(false);
    };
    let registered = match spec.registration {
        Registration::DirectoryLoaded => true,
        Registration::Grok { file, timeout } => {
            grok_hook_config_is_valid(&dir.join("hooks").join(file), hook_path, timeout)?
        }
        Registration::Opencode => {
            return opencode_tui_integration_is_valid(
                hook_path,
                &paths.opencode_state_directory()?,
            );
        }
        Registration::Kimi { file, timeout } => {
            kimi_hooks_registered(&dir.join(file), hook_path, timeout)?
        }
        Registration::AntigravityCli { file, timeout } => {
            let expected_block =
                super::targets::antigravity_cli_hook_block_with_timeout(hook_path, timeout)?;
            read_json(&dir.join(file))?.is_some_and(|document| {
                document.get(super::ANTIGRAVITY_CLI_HOOK_BLOCK_NAME) == Some(&expected_block)
            })
        }
        Registration::Codex {
            hooks,
            config,
            timeout,
            required_fields,
            event_policy,
            ..
        } => {
            json_hook_commands_registered(
                &dir.join(hooks),
                HooksRoot::HooksKey,
                &JsonShape::Nested(timeout).expected_events(
                    spec.target,
                    hook_path,
                    event_policy,
                )?,
                required_fields,
                hook_path,
            )? && codex_hooks_feature_enabled(&dir.join(config))?
        }
        Registration::Json {
            file,
            root,
            shape,
            required_fields,
            event_policy,
            ..
        } => json_hook_commands_registered(
            &dir.join(file),
            root,
            &shape.expected_events(spec.target, hook_path, event_policy)?,
            required_fields,
            hook_path,
        )?,
    };
    Ok(registered)
}

fn file_matches_asset(path: &Path, asset: &str) -> InstallResult<bool> {
    let installed = super::file_ops::is_file(path).map_err(|error| {
        InstallError::from(io::Error::new(
            error.io_kind(),
            format!("cannot stat {}: {error}", path.display()),
        ))
    })?;
    if !installed {
        return Ok(false);
    }
    let content = fs::read(path).map_err(|error| {
        InstallError::from(io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", path.display()),
        ))
    })?;
    Ok(content.as_slice() == asset.as_bytes())
}

fn integration_state_for_path(
    path: &Path,
    expected_asset: &str,
) -> InstallResult<(Option<bool>, Option<u32>)> {
    let installed = super::file_ops::is_file(path).map_err(|error| {
        InstallError::from(io::Error::new(
            error.io_kind(),
            format!("cannot stat {}: {error}", path.display()),
        ))
    })?;
    if !installed {
        return Ok((None, None));
    }

    let content = fs::read(path).map_err(|error| {
        InstallError::from(io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", path.display()),
        ))
    })?;
    let installed_version = std::str::from_utf8(&content)
        .ok()
        .and_then(parse_integration_version);
    // Only release launches install these shared artifacts. Exact bytes detect
    // edits; the version marker is only reported.
    // Dev launches must skip status-driven installation altogether.
    Ok((
        Some(content.as_slice() == expected_asset.as_bytes()),
        installed_version,
    ))
}

/// The status of the integration installed at `path`. A stat or read error on
/// its asset, or a stat, read or parse error on its registration config, is
/// returned. A missing registration reads `Outdated`, so the next install
/// repairs it.
fn integration_status_at_with_paths(
    target: shepr_agent::IntegrationTarget,
    path: PathBuf,
    paths: &AgentIntegrationPaths,
) -> Result<super::IntegrationStatus, InstallError> {
    let spec = spec_for(target);
    let expected_asset = spec.primary_asset.contents;
    let (asset_current, installed_version) = integration_state_for_path(&path, expected_asset)?;
    let Some(asset_current) = asset_current else {
        return Ok(super::IntegrationStatus {
            target,
            path,
            state: super::IntegrationStatusKind::NotInstalled,
            outdated_reason: None,
            installed_version,
        });
    };
    let registration_current = hook_registration_is_current(spec, &path, paths)?;
    let (state, outdated_reason) = match (asset_current, registration_current) {
        (true, true) => (super::IntegrationStatusKind::Current, None),
        (true, false) => (
            super::IntegrationStatusKind::Outdated,
            Some(IntegrationOutdatedReason::Registration),
        ),
        (false, true) => (
            super::IntegrationStatusKind::Outdated,
            Some(IntegrationOutdatedReason::Asset),
        ),
        (false, false) => (
            super::IntegrationStatusKind::Outdated,
            Some(IntegrationOutdatedReason::AssetAndRegistration),
        ),
    };

    Ok(super::IntegrationStatus {
        target,
        path,
        state,
        outdated_reason,
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

#[cfg(test)]
pub(crate) fn integration_hook_timeout(target: Target) -> InstallResult<Duration> {
    spec_for(target).registration.timeout().ok_or_else(|| {
        InstallError::from(io::Error::other(format!(
            "{target:?} does not register timed hooks"
        )))
    })
}

#[cfg(test)]
pub(crate) fn integration_asset(target: Target) -> Option<&'static str> {
    Some(spec_for(target).primary_asset.contents)
}

/// `integration_status_at_with_paths` with the paths resolved from the
/// process environment.
#[cfg(test)]
pub(crate) fn integration_status_at(
    target: shepr_agent::IntegrationTarget,
    path: PathBuf,
) -> Result<super::IntegrationStatus, InstallError> {
    let paths = AgentIntegrationPaths::resolve();
    integration_status_at_with_paths(target, path, &paths)
}

/// One status per supported target, in descriptor order.
#[cfg(test)]
pub(crate) fn integration_status_rows(
    paths: &AgentIntegrationPaths,
) -> Vec<Result<super::IntegrationStatus, InstallError>> {
    Target::all()
        .map(|target| integration_status(paths, target))
        .collect()
}

#[cfg(test)]
mod registration_tests {
    use super::super::command::hook_command;
    use super::*;
    use crate::IntegrationStatusKind;
    use crate::types::InstallErrorKind;
    use shepr_agent::IntegrationTarget;

    #[test]
    fn antigravity_integration_uses_the_canonical_agent_label() {
        assert_eq!(
            IntegrationTarget::AntigravityCli.label(),
            shepr_agent::Agent::Antigravity.label()
        );
    }

    /// Status compares the installed files with the bundled bytes and the
    /// agent config with what install writes, so every target must read
    /// Current straight after its own install.
    #[test]
    fn every_target_reads_current_right_after_install() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = AgentIntegrationPaths::resolve();
        for spec in Target::all().map(spec_for) {
            let label = spec.target.label();
            let agent_directory = agent_directory(&paths, spec.target).expect("test precondition");
            fs::create_dir_all(&agent_directory).expect("test precondition");
            super::super::targets::install(&paths, spec.target)
                .unwrap_or_else(|error| panic!("{label} install failed: {error}"));
            let status = integration_status(&paths, spec.target)
                .unwrap_or_else(|error| panic!("{label} status failed: {error}"));
            assert_eq!(status.state, IntegrationStatusKind::Current, "{label}");
        }
    }

    #[test]
    fn every_config_target_is_checked_before_any_asset_changes() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = AgentIntegrationPaths::resolve();
        for target in Target::all() {
            let dir = target_directory(&paths, target).expect("target directory");
            let config_paths = registration(target).config_paths(&dir);
            let Some(config_path) = config_paths.first() else {
                continue;
            };
            fs::create_dir_all(agent_directory(&paths, target).expect("agent directory"))
                .expect("create agent directory");
            fs::create_dir_all(config_path).expect("occupy config with directory");
            let hook_path = target_path(&paths, target).expect("hook path");
            fs::create_dir_all(hook_path.parent().expect("hook parent"))
                .expect("create hook parent");
            fs::write(&hook_path, "previous hook").expect("write previous hook");

            assert!(
                super::super::targets::install(&paths, target).is_err(),
                "{target:?}"
            );
            assert_eq!(
                fs::read_to_string(&hook_path).expect("read previous hook"),
                "previous hook",
                "{target:?}"
            );
            assert!(
                super::super::file_ops::is_dir(config_path).expect("config remains a directory")
            );
        }
    }

    #[test]
    fn codex_prepares_both_configs_before_publishing_either_or_the_hook() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = AgentIntegrationPaths::resolve();
        let dir = target_directory(&paths, Target::Codex).expect("Codex directory");
        fs::create_dir_all(&dir).expect("create Codex directory");
        let hook = target_path(&paths, Target::Codex).expect("hook path");
        let hooks = dir.join(super::super::CODEX_HOOKS_NAME);
        let config = dir.join(super::super::CODEX_CONFIG_NAME);
        fs::write(&hook, "previous hook").expect("write hook");
        fs::write(&hooks, "{}\n").expect("write hooks config");
        fs::write(&config, "[broken\n").expect("write malformed TOML config");

        assert!(super::super::targets::install(&paths, Target::Codex).is_err());
        assert_eq!(
            fs::read_to_string(hook).expect("read hook"),
            "previous hook"
        );
        assert_eq!(fs::read_to_string(hooks).expect("read hooks"), "{}\n");
        assert_eq!(
            fs::read_to_string(config).expect("read config"),
            "[broken\n"
        );
    }

    #[test]
    fn codex_disabled_hooks_are_refused_without_publishing_anything() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = AgentIntegrationPaths::resolve();
        let dir = target_directory(&paths, Target::Codex).expect("Codex directory");
        fs::create_dir_all(&dir).expect("create Codex directory");
        let hook = target_path(&paths, Target::Codex).expect("hook path");
        let hooks = dir.join(super::super::CODEX_HOOKS_NAME);
        let config = dir.join(super::super::CODEX_CONFIG_NAME);
        let original = "model = \"x\"\n[features]\nhooks = false\ncodex_hooks = true\n";
        fs::write(&config, original).expect("write user's config");

        let error = super::super::targets::install(&paths, Target::Codex)
            .expect_err("explicit global opt-out must be respected");

        assert!(error.to_string().contains("features.hooks = false"));
        assert_eq!(fs::read_to_string(&config).expect("read config"), original);
        assert!(
            !hooks.try_exists().expect("stat hooks config"),
            "hooks config must not be published"
        );
        assert!(
            !hook.try_exists().expect("stat hook asset"),
            "hook asset must not be published"
        );
    }

    #[test]
    fn grok_status_marks_malformed_owned_hook_config_outdated_and_repairs_it() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = AgentIntegrationPaths::resolve();
        let dir = target_directory(&paths, Target::Grok).expect("Grok directory");
        fs::create_dir_all(agent_directory(&paths, Target::Grok).expect("agent directory"))
            .expect("create Grok directory");
        super::super::targets::install(&paths, Target::Grok).expect("initial install");

        let config = dir.join("hooks").join(super::super::GROK_HOOK_CONFIG_NAME);
        fs::write(&config, "{ truncated").expect("corrupt owned hook config");
        let status = integration_status(&paths, Target::Grok).expect("status malformed config");
        assert_eq!(status.state, IntegrationStatusKind::Outdated);

        super::super::targets::install(&paths, Target::Grok)
            .expect("repair malformed owned config");
        assert_eq!(
            integration_status(&paths, Target::Grok)
                .expect("status repaired config")
                .state,
            IntegrationStatusKind::Current
        );
    }

    #[test]
    fn bundled_integration_specs_register_their_assets() {
        for spec in Target::all().map(spec_for) {
            assert!(
                !spec.primary_asset.contents.is_empty(),
                "{} must register its bundled assets",
                spec.target.label()
            );
            for (index, asset) in std::iter::once(&spec.primary_asset)
                .chain(spec.additional_assets.iter())
                .enumerate()
            {
                assert!(
                    parse_integration_version(asset.contents).is_some(),
                    "{} bundled asset {index} must carry diagnostic version metadata",
                    spec.target.label()
                );
            }
        }
    }

    #[test]
    fn bundled_integration_assets_report_the_descriptor_identity() {
        for spec in Target::all().map(spec_for) {
            let agent = spec.target.agent();
            let source = agent
                .integration_source()
                .expect("integration targets must have a source");
            for (index, asset) in std::iter::once(&spec.primary_asset)
                .chain(spec.additional_assets.iter())
                .enumerate()
            {
                // OpenCode V2 re-exports the TUI reporter, so its identity lives in that asset.
                if spec.target == Target::Opencode
                    && asset.contents == super::super::OPENCODE_V2_TUI_PLUGIN_ASSET
                {
                    continue;
                }
                assert!(
                    asset.contents.contains(source),
                    "{} bundled asset {index} must report source {source:?}",
                    agent.label()
                );
                assert!(
                    asset.contents.contains(agent.label()),
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
        let paths = crate::AgentIntegrationPaths::resolve();

        let rows = integration_status_rows(&paths);
        assert_eq!(
            rows.len(),
            IntegrationTarget::all().count(),
            "one row per target"
        );
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
        assert_eq!(error.kind(), InstallErrorKind::NotRegularFile);
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
        assert_eq!(
            integration_status_at(IntegrationTarget::Claude, hook.clone())
                .expect("stat hook")
                .outdated_reason,
            Some(IntegrationOutdatedReason::Registration)
        );

        let settings_path = dir.join("settings.json");
        let target = Target::Claude;
        let installed = super::super::json_edit::install_claude_settings(
            "{}",
            &settings_path,
            &hook,
            integration_hook_timeout(target).expect("test precondition"),
        )
        .expect("test precondition");
        fs::write(&settings_path, installed).expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Claude, &hook),
            IntegrationStatusKind::Current
        );

        fs::write(&hook, "edited managed asset").expect("test precondition");
        assert_eq!(
            integration_status_at(IntegrationTarget::Claude, hook.clone())
                .expect("stat edited hook")
                .outdated_reason,
            Some(IntegrationOutdatedReason::Asset)
        );
        fs::write(&hook, super::super::CLAUDE_HOOK_ASSET).expect("restore managed asset");

        // The user deleting the entry leaves the hook file current but inert.
        fs::write(&settings_path, "{\"hooks\":{}}").expect("test precondition");
        assert_eq!(
            state(IntegrationTarget::Claude, &hook),
            IntegrationStatusKind::Outdated
        );
        fs::write(&hook, "edited managed asset").expect("test precondition");
        assert_eq!(
            integration_status_at(IntegrationTarget::Claude, hook.clone())
                .expect("stat edited hook")
                .outdated_reason,
            Some(IntegrationOutdatedReason::AssetAndRegistration)
        );
        fs::write(&hook, super::super::CLAUDE_HOOK_ASSET).expect("restore managed asset");
        fs::write(&settings_path, "{ not json").expect("test precondition");
        let error = integration_status_at(IntegrationTarget::Claude, hook.clone())
            .expect_err("invalid config must be reported");
        assert!(error.to_string().contains("cannot parse"));

        fs::remove_file(&settings_path).expect("test precondition");
        fs::create_dir(&settings_path).expect("test precondition");
        let error = integration_status_at(IntegrationTarget::Claude, hook)
            .expect_err("config read failure must be reported");
        assert_eq!(error.kind(), InstallErrorKind::NotRegularFile);
    }

    #[test]
    fn claude_command_outside_the_installed_shape_is_outdated() {
        let dir = base("claude-shape");
        let hook = dir.join("hooks").join("shepr-agent-state.sh");
        write_current_hook(IntegrationTarget::Claude, &hook);
        let settings_path = dir.join("settings.json");
        let command = hook_command(&hook, Some("session"));
        let matcher = super::super::registration::claude_session_start_matcher();
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
                { "hooks": [{ "type": "command", "command": hook_command(&hook, Some(action)), "timeout": super::super::HOOK_TIMEOUT.as_secs() }] }
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
        super::super::targets::install(&paths, IntegrationTarget::Kimi).expect("install");

        let status = integration_status(&paths, IntegrationTarget::Kimi).expect("status");
        assert_eq!(status.state, IntegrationStatusKind::Current);
        let hook = status.path;
        let config_path = dir.join(super::super::KIMI_CONFIG_NAME);
        let config = fs::read_to_string(&config_path).expect("test precondition");
        let stale_registration = format!(
            "[[hooks]]\nevent = \"OldEvent\"\ncommand = {}\ntimeout = {}\n\n{}",
            super::super::config_edit::toml_basic_string(&hook_command(&hook, Some("old-action"),)),
            super::super::HOOK_TIMEOUT.as_secs(),
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

        super::super::targets::install(&paths, IntegrationTarget::Kimi).expect("reinstall");
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
    fn kimi_status_and_install_reject_a_hook_outside_the_managed_block() {
        let env = shepr_test_support::IsolatedEnv::new();
        let dir = env.home().join(".kimi-code");
        fs::create_dir_all(&dir).expect("test precondition");
        let paths = AgentIntegrationPaths::resolve();
        super::super::targets::install(&paths, IntegrationTarget::Kimi).expect("install");

        let status = integration_status(&paths, IntegrationTarget::Kimi).expect("status");
        let hook = status.path;
        let config_path = dir.join(super::super::KIMI_CONFIG_NAME);
        let config = fs::read_to_string(&config_path).expect("test precondition");
        let external_hook = format!(
            "[[hooks]]\nevent = \"SessionStart\"\ncommand = {}\ntimeout = {}\n\n",
            super::super::config_edit::toml_basic_string(&hook_command(&hook, Some("session"))),
            super::super::HOOK_TIMEOUT.as_secs()
        );
        let config_with_external_hook = format!("{external_hook}{config}");
        fs::write(&config_path, &config_with_external_hook).expect("test precondition");

        assert_eq!(
            integration_status(&paths, IntegrationTarget::Kimi)
                .expect("status")
                .state,
            IntegrationStatusKind::Outdated
        );
        let error = super::super::targets::install(&paths, IntegrationTarget::Kimi)
            .expect_err("an unmarked Shepr hook must not be installed twice")
            .to_string();
        assert!(error.contains("outside its managed block"), "{error}");
        assert_eq!(
            fs::read_to_string(&config_path).expect("test precondition"),
            config_with_external_hook
        );
    }

    #[test]
    fn mastracode_checks_flat_top_level_events() {
        let dir = base("mastracode");
        let hook = dir.join("hooks").join("shepr-agent-state.sh");
        write_current_hook(IntegrationTarget::Mastracode, &hook);
        let mut document = serde_json::Map::new();
        let timeout_millis = super::super::registration::timeout_millis(super::super::HOOK_TIMEOUT)
            .expect("test precondition");
        for event_spec in IntegrationTarget::Mastracode.hook_events() {
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
        let result = super::super::targets::install(
            &super::super::env::AgentIntegrationPaths::resolve(),
            IntegrationTarget::Claude,
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
        let result = super::super::targets::install(
            &super::super::env::AgentIntegrationPaths::resolve(),
            IntegrationTarget::Codex,
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
        let result = super::super::targets::install(
            &super::super::env::AgentIntegrationPaths::resolve(),
            IntegrationTarget::Copilot,
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

        type Install = fn(&super::super::env::AgentIntegrationPaths) -> InstallResult<()>;
        let env = shepr_test_support::IsolatedEnv::new();
        let home = env.home();
        let cases: [(IntegrationTarget, &[&str], &str, HooksRoot, Install); 7] = [
            (
                IntegrationTarget::Claude,
                &[".claude"],
                "settings.json",
                HooksRoot::HooksKey,
                |paths| targets::install(paths, IntegrationTarget::Claude).map(|_| ()),
            ),
            (
                IntegrationTarget::Codex,
                &[".codex"],
                "hooks.json",
                HooksRoot::HooksKey,
                |paths| targets::install(paths, IntegrationTarget::Codex).map(|_| ()),
            ),
            (
                IntegrationTarget::Copilot,
                &[".copilot"],
                "settings.json",
                HooksRoot::HooksKey,
                |paths| targets::install(paths, IntegrationTarget::Copilot).map(|_| ()),
            ),
            (
                IntegrationTarget::Devin,
                &[".config", "devin"],
                "config.json",
                HooksRoot::HooksKey,
                |paths| targets::install(paths, IntegrationTarget::Devin).map(|_| ()),
            ),
            (
                IntegrationTarget::Droid,
                &[".factory"],
                "settings.json",
                HooksRoot::HooksKey,
                |paths| targets::install(paths, IntegrationTarget::Droid).map(|_| ()),
            ),
            (
                IntegrationTarget::Cursor,
                &[".cursor"],
                "hooks.json",
                HooksRoot::HooksKey,
                |paths| targets::install(paths, IntegrationTarget::Cursor).map(|_| ()),
            ),
            (
                IntegrationTarget::Mastracode,
                &[".mastracode"],
                "hooks.json",
                HooksRoot::Document,
                |paths| targets::install(paths, IntegrationTarget::Mastracode).map(|_| ()),
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
    fn every_target_has_its_exhaustive_spec() {
        for target in IntegrationTarget::all() {
            assert_eq!(spec_for(target).target, target);
        }
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
