use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::agent::{IntegrationHookAction, IntegrationTarget as Target};

use super::claude_settings::{
    install as install_claude_settings, uninstall as uninstall_claude_settings,
};
use super::command::{hook_command, hook_command_with_interpreter};
use super::config_edit::{
    build_codex_config_with_hooks, build_kimi_config_with_hooks, ensure_command_hook,
    ensure_direct_command_hook, ensure_flat_command_hook, ensure_hooks_object,
    ensure_simple_command_hook, hooks_object_if_present, remove_direct_hook_commands,
    remove_flat_command_hook, remove_hook_commands, remove_kimi_config_block,
    remove_simple_command_hook,
};
use super::config_file::{check_config_targets, lock_config_for_update, write_config};
use super::env::{AgentIntegrationPaths, DirectoryKey};
use super::file_ops::{
    is_dir, is_file, remove_dir_all_if_exists, remove_file_if_exists, write_managed_asset,
};
use super::opencode_config::{
    PluginConfigEdit, prepare_cli_plugin, prepare_tui_plugin, remove_cli_plugin, remove_tui_plugin,
    validate_tui_plugin_config,
};
use super::registry::{config_file_names, integration_hook_events, integration_hook_timeout};
use super::types::{ArtifactRole, InstallOutcome, UninstallOutcome, UninstallState};
use super::{
    ANTIGRAVITY_CLI_HOOK_BLOCK_NAME, ANTIGRAVITY_CLI_HOOK_INSTALL_NAME, CLAUDE_HOOK_INSTALL_NAME,
    CODEX_HOOK_INSTALL_NAME, COPILOT_HOOK_INSTALL_NAME, CURSOR_HOOK_INSTALL_NAME,
    DEVIN_HOOK_INSTALL_NAME, DROID_HOOK_INSTALL_NAME, GROK_HOOK_INSTALL_NAME,
    KILO_PLUGIN_INSTALL_NAME, KIMI_HOOK_INSTALL_NAME, KIMI_MIN_VERSION,
    MASTRACODE_HOOK_INSTALL_NAME, OMP_EXTENSION_INSTALL_NAME, OPENCODE_PLUGIN_INSTALL_NAME,
    OPENCODE_TUI_PLUGIN_ASSET, OPENCODE_TUI_PLUGIN_INSTALL_NAME, OPENCODE_TUI_PLUGIN_SPEC,
    PI_EXTENSION_INSTALL_NAME,
};

// Install order for targets that register the hook in an agent config: read,
// parse and edit the config in memory first, then write the hook script, then
// the config. A config that cannot be edited then fails the install before
// anything is written, instead of leaving a hook script that `integration
// status` would see while the agent never runs it.

/// Write one asset described by the integration spec via temp-file-and-rename.
fn write_target_asset(target: Target, path: &Path, executable: bool) -> io::Result<()> {
    let asset = super::registry::integration_asset(target)
        .ok_or_else(|| io::Error::other(format!("missing asset for integration {target:?}")))?;
    write_managed_asset(path, asset.as_bytes(), executable)
}

fn write_hook_script(target: Target, path: &Path) -> io::Result<()> {
    write_target_asset(target, path, true)
}

fn read_json_config(path: &Path, default: Value) -> io::Result<Value> {
    if !is_file(path)? {
        return Ok(default);
    }
    serde_json::from_str::<Value>(&fs::read_to_string(path)?)
        .map_err(|err| io::Error::other(format!("failed to parse {}: {err}", path.display())))
}

fn ensure_extension_dir(dir: &Path, agent: &str) -> io::Result<()> {
    if is_dir(dir)? {
        return Ok(());
    }
    let parent_is_dir = match dir.parent() {
        Some(parent) => is_dir(parent)?,
        None => false,
    };
    if parent_is_dir {
        return fs::create_dir_all(dir);
    }
    Err(io::Error::other(format!(
        "{agent} extension directory not found at {}. install {agent} first",
        dir.display()
    )))
}

fn timeout_millis(timeout: Duration) -> io::Result<u64> {
    u64::try_from(timeout.as_millis())
        .map_err(|_| io::Error::other("hook timeout exceeds millisecond configuration range"))
}

pub(crate) fn install_pi(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::PiExtension)?;
    ensure_extension_dir(&dir, "pi")?;

    let path = dir.join(PI_EXTENSION_INSTALL_NAME);
    write_target_asset(Target::Pi, &path, false)?;
    Ok(InstallOutcome::default().with_artifact(ArtifactRole::Extension, path))
}

pub(crate) fn install_omp(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::OmpExtension)?;
    let pi_dir = paths.directory(DirectoryKey::PiExtension)?;
    if dir == pi_dir {
        return Err(io::Error::other(format!(
            "Pi and OMP resolve to the same extension directory at {}; configure separate agent directories before installing OMP",
            dir.display()
        )));
    }
    ensure_extension_dir(&dir, "omp")?;

    let extension_path = dir.join(OMP_EXTENSION_INSTALL_NAME);
    write_target_asset(Target::Omp, &extension_path, false)?;
    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Extension, extension_path);
    Ok(outcome)
}

pub(crate) fn install_claude(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::Claude)?;
    check_config_targets(&dir, config_file_names(Target::Claude)?)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "claude directory not found at {}. install claude code first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);

    let settings_path = dir.join(super::CLAUDE_SETTINGS_NAME);
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let existing_settings = if is_file(&settings_path)? {
        fs::read_to_string(&settings_path)?
    } else {
        "{}".to_string()
    };
    // Edit the settings in memory before writing anything, so settings that
    // cannot be parsed or edited leave no orphan hook behind.
    let updated_settings = install_claude_settings(
        &existing_settings,
        &settings_path,
        &hook_path,
        integration_hook_events(Target::Claude),
        integration_hook_timeout(Target::Claude)?,
    )?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::Claude, &hook_path)?;

    if updated_settings != existing_settings {
        write_config(&settings_path, updated_settings)?;
    }

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Hook, hook_path);
    outcome = outcome.with_artifact(ArtifactRole::Settings, settings_path);
    Ok(outcome)
}

pub(crate) fn install_codex(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::Codex)?;
    check_config_targets(&dir, config_file_names(Target::Codex)?)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "codex config directory not found at {}. install codex first",
            dir.display()
        )));
    }

    let hook_path = dir.join(CODEX_HOOK_INSTALL_NAME);

    let hooks_path = dir.join(super::CODEX_HOOKS_NAME);
    let _hooks_lock = lock_config_for_update(&hooks_path)?;
    let mut hooks_file = read_json_config(&hooks_path, json!({}))?;

    let hooks = ensure_hooks_object(
        &mut hooks_file,
        &hooks_path,
        "codex hooks file",
        "codex hooks file hooks",
    )?;
    remove_hook_commands(hooks, "SessionStart", &hook_path, Some("session"))?;
    ensure_command_hook(
        hooks,
        "SessionStart",
        &hook_command(&hook_path, Some("session")),
        integration_hook_timeout(Target::Codex)?.as_secs(),
        None,
    )?;
    let hooks_contents = serde_json::to_string_pretty(&hooks_file)?;

    let config_path = dir.join(super::CODEX_CONFIG_NAME);
    let _config_lock = lock_config_for_update(&config_path)?;
    let existing_config = if is_file(&config_path)? {
        fs::read_to_string(&config_path)?
    } else {
        String::new()
    };
    let new_config = build_codex_config_with_hooks(&existing_config)?;

    write_hook_script(Target::Codex, &hook_path)?;
    write_config(&hooks_path, hooks_contents)?;
    if new_config != existing_config {
        write_config(&config_path, new_config)?;
    }

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Hook, hook_path);
    outcome = outcome.with_artifact(ArtifactRole::Hooks, hooks_path);
    outcome = outcome.with_artifact(ArtifactRole::Config, config_path);
    Ok(outcome)
}

pub(crate) fn install_kimi(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::Kimi)?;
    check_config_targets(&dir, config_file_names(Target::Kimi)?)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "kimi code config directory not found at {}. install kimi code first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(KIMI_HOOK_INSTALL_NAME);
    let config_path = dir.join(super::KIMI_CONFIG_NAME);
    let _config_lock = lock_config_for_update(&config_path)?;
    let existing_config = if is_file(&config_path)? {
        fs::read_to_string(&config_path)?
    } else {
        String::new()
    };
    // Build the new config before touching the hook so a config that cannot
    // be edited safely leaves nothing installed.
    let new_config = build_kimi_config_with_hooks(&existing_config, &hook_path)?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::Kimi, &hook_path)?;

    if new_config != existing_config {
        write_config(&config_path, new_config)?;
    }

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Hook, hook_path);
    outcome = outcome.with_artifact(ArtifactRole::Config, config_path);
    outcome = outcome.with_notice(format!("requires kimi code {KIMI_MIN_VERSION} or newer"));
    Ok(outcome)
}

pub(crate) fn install_copilot(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::Copilot)?;
    check_config_targets(&dir, config_file_names(Target::Copilot)?)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "copilot config directory not found at {}. install github copilot cli first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(COPILOT_HOOK_INSTALL_NAME);

    let settings_path = dir.join(super::COPILOT_SETTINGS_NAME);
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut settings = read_json_config(&settings_path, json!({}))?;

    let hooks = ensure_hooks_object(
        &mut settings,
        &settings_path,
        "copilot settings",
        "copilot settings hooks",
    )?;
    for hook in integration_hook_events(Target::Copilot) {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        remove_direct_hook_commands(hooks, hook.event, &hook_path, action)?;
    }
    for hook in integration_hook_events(Target::Copilot) {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        ensure_direct_command_hook(
            hooks,
            hook.event,
            hook_command(&hook_path, action),
            integration_hook_timeout(Target::Copilot)?.as_secs(),
            None,
        )?;
    }
    let settings_contents = serde_json::to_string_pretty(&settings)?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::Copilot, &hook_path)?;
    write_config(&settings_path, settings_contents)?;

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Hook, hook_path);
    outcome = outcome.with_artifact(ArtifactRole::Settings, settings_path);
    Ok(outcome)
}

pub(crate) fn install_devin(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::Devin)?;
    check_config_targets(&dir, config_file_names(Target::Devin)?)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "devin config directory not found at {}. install devin cli first",
            dir.display()
        )));
    }

    let hook_path = dir.join(DEVIN_HOOK_INSTALL_NAME);

    let settings_path = dir.join(super::DEVIN_CONFIG_NAME);
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut settings = read_json_config(&settings_path, json!({}))?;

    let hooks = ensure_hooks_object(
        &mut settings,
        &settings_path,
        "devin settings",
        "devin settings hooks",
    )?;
    for hook in integration_hook_events(Target::Devin) {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        remove_hook_commands(hooks, hook.event, &hook_path, action)?;
    }
    for hook in integration_hook_events(Target::Devin) {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        ensure_command_hook(
            hooks,
            hook.event,
            &hook_command(&hook_path, action),
            integration_hook_timeout(Target::Devin)?.as_secs(),
            None,
        )?;
    }
    let settings_contents = serde_json::to_string_pretty(&settings)?;

    write_hook_script(Target::Devin, &hook_path)?;
    write_config(&settings_path, settings_contents)?;

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Hook, hook_path);
    outcome = outcome.with_artifact(ArtifactRole::Settings, settings_path);
    Ok(outcome)
}

pub(crate) fn install_droid(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::Droid)?;
    check_config_targets(&dir, config_file_names(Target::Droid)?)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "droid config directory not found at {}. install droid first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(DROID_HOOK_INSTALL_NAME);

    let settings_path = dir.join(super::DROID_SETTINGS_NAME);
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut settings = read_json_config(&settings_path, json!({}))?;

    let hooks = ensure_hooks_object(
        &mut settings,
        &settings_path,
        "droid settings",
        "droid settings hooks",
    )?;
    for hook in integration_hook_events(Target::Droid) {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        remove_hook_commands(hooks, hook.event, &hook_path, action)?;
    }
    for hook in integration_hook_events(Target::Droid) {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        ensure_command_hook(
            hooks,
            hook.event,
            &hook_command(&hook_path, action),
            integration_hook_timeout(Target::Droid)?.as_secs(),
            None,
        )?;
    }
    let settings_contents = serde_json::to_string_pretty(&settings)?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::Droid, &hook_path)?;
    write_config(&settings_path, settings_contents)?;

    // Droid keeps its hooks in settings.json; the operator is told about hooks.
    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Hook, hook_path);
    outcome = outcome.with_artifact(ArtifactRole::Hooks, settings_path);
    Ok(outcome)
}

pub(crate) fn install_opencode(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::Opencode)?;
    check_config_targets(&dir, config_file_names(Target::Opencode)?)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "opencode config directory not found at {}. install opencode first",
            dir.display()
        )));
    }

    validate_tui_plugin_config(&dir)?;
    let tui_config_edit = prepare_tui_plugin(&dir, OPENCODE_TUI_PLUGIN_SPEC)?;
    let cli_config_edit = prepare_cli_plugin(
        &dir,
        &paths.directory(DirectoryKey::OpencodeState)?,
        super::OPENCODE_V2_TUI_PLUGIN_SPEC,
    )?;

    let plugins_dir = dir.join("plugins");
    fs::create_dir_all(&plugins_dir)?;

    let plugin_path = plugins_dir.join(OPENCODE_PLUGIN_INSTALL_NAME);
    write_target_asset(Target::Opencode, &plugin_path, false)?;
    let tui_plugin_path = dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME);
    write_managed_asset(
        &tui_plugin_path,
        OPENCODE_TUI_PLUGIN_ASSET.as_bytes(),
        false,
    )?;
    let v2_dir = dir.join(super::OPENCODE_V2_TUI_PLUGIN_DIR);
    fs::create_dir_all(&v2_dir)?;
    write_managed_asset(
        &v2_dir.join("tui.js"),
        super::OPENCODE_V2_TUI_PLUGIN_ASSET.as_bytes(),
        false,
    )?;
    // Both configs were parsed and edited in memory before any plugin files
    // were written; publish their edits after the assets are in place.
    let tui_config_path = tui_config_edit.write()?;
    let cli_config_path = cli_config_edit.map(PluginConfigEdit::write).transpose()?;

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Plugin, plugin_path);
    outcome = outcome.with_artifact(ArtifactRole::TuiPlugin, tui_plugin_path);
    outcome = outcome.with_artifact(ArtifactRole::TuiConfig, tui_config_path);
    if cli_config_path.is_none() {
        outcome = outcome.with_notice(
            "to enable OpenCode V2, start opencode2 once, then reinstall this integration"
                .to_string(),
        );
    }
    Ok(outcome)
}

pub(crate) fn install_kilo(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::Kilo)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "kilo config directory not found at {}. install kilo first",
            dir.display()
        )));
    }

    let plugins_dir = dir.join("plugin");
    fs::create_dir_all(&plugins_dir)?;

    let plugin_path = plugins_dir.join(KILO_PLUGIN_INSTALL_NAME);
    write_target_asset(Target::Kilo, &plugin_path, false)?;

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Plugin, plugin_path);
    Ok(outcome)
}

pub(crate) fn uninstall_pi(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let extension_path = paths
        .directory(DirectoryKey::PiExtension)?
        .join(PI_EXTENSION_INSTALL_NAME);
    let removed_extension = remove_file_if_exists(&extension_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Extension, extension_path, removed_extension);
    Ok(outcome)
}

pub(crate) fn uninstall_omp(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let extension_path = paths
        .directory(DirectoryKey::OmpExtension)?
        .join(OMP_EXTENSION_INSTALL_NAME);
    let removed_extension = remove_file_if_exists(&extension_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Extension, extension_path, removed_extension);
    Ok(outcome)
}

pub(crate) fn uninstall_claude(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let dir = paths.directory(DirectoryKey::Claude)?;
    check_config_targets(&dir, config_file_names(Target::Claude)?)?;
    let hook_path = dir.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME);
    let settings_path = dir.join(super::CLAUDE_SETTINGS_NAME);
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut updated_settings = false;

    if is_file(&settings_path)? {
        let existing_settings = fs::read_to_string(&settings_path)?;
        let new_settings = uninstall_claude_settings(
            &existing_settings,
            &settings_path,
            &hook_path,
            integration_hook_events(Target::Claude),
            integration_hook_timeout(Target::Claude)?,
        )?;
        updated_settings = new_settings != existing_settings;
        if updated_settings {
            write_config(&settings_path, new_settings)?;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Hook, hook_path, removed_hook_file);
    outcome.record_update(ArtifactRole::Settings, settings_path, updated_settings);
    Ok(outcome)
}

pub(crate) fn uninstall_codex(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let codex_dir = paths.directory(DirectoryKey::Codex)?;
    // Uninstall leaves `config.toml` and its hooks feature switch alone
    // (reported as preserved below), so only the file it edits is vetted.
    check_config_targets(&codex_dir, &[super::CODEX_HOOKS_NAME])?;
    let hook_path = codex_dir.join(CODEX_HOOK_INSTALL_NAME);
    let hooks_path = codex_dir.join(super::CODEX_HOOKS_NAME);
    let config_path = codex_dir.join(super::CODEX_CONFIG_NAME);
    let _hooks_lock = lock_config_for_update(&hooks_path)?;
    let mut updated_hooks = false;

    if is_file(&hooks_path)? {
        let mut hooks_file = serde_json::from_str::<Value>(&fs::read_to_string(&hooks_path)?)
            .map_err(|err| {
                io::Error::other(format!("failed to parse {}: {err}", hooks_path.display()))
            })?;

        if let Some(hooks) = hooks_object_if_present(
            &mut hooks_file,
            &hooks_path,
            "codex hooks file",
            "codex hooks file hooks",
        )? {
            updated_hooks |=
                remove_hook_commands(hooks, "SessionStart", &hook_path, Some("session"))?;
        }

        if updated_hooks {
            write_config(&hooks_path, serde_json::to_string_pretty(&hooks_file)?)?;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Hook, hook_path, removed_hook_file);
    outcome.record_update(ArtifactRole::Hooks, hooks_path, updated_hooks);
    outcome.record(ArtifactRole::Config, config_path, UninstallState::Preserved);
    Ok(outcome)
}

pub(crate) fn uninstall_kimi(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let kimi_dir = paths.directory(DirectoryKey::Kimi)?;
    check_config_targets(&kimi_dir, config_file_names(Target::Kimi)?)?;
    let hook_path = kimi_dir.join("hooks").join(KIMI_HOOK_INSTALL_NAME);
    let config_path = kimi_dir.join(super::KIMI_CONFIG_NAME);
    let _config_lock = lock_config_for_update(&config_path)?;
    let mut updated_config = false;

    if is_file(&config_path)? {
        let existing_config = fs::read_to_string(&config_path)?;
        let new_config = remove_kimi_config_block(&existing_config)?;
        if new_config != existing_config {
            write_config(&config_path, new_config)?;
            updated_config = true;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Hook, hook_path, removed_hook_file);
    outcome.record_update(ArtifactRole::Config, config_path, updated_config);
    Ok(outcome)
}

pub(crate) fn uninstall_copilot(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let copilot_dir = paths.directory(DirectoryKey::Copilot)?;
    check_config_targets(&copilot_dir, config_file_names(Target::Copilot)?)?;
    let hook_path = copilot_dir.join("hooks").join(COPILOT_HOOK_INSTALL_NAME);
    let settings_path = copilot_dir.join(super::COPILOT_SETTINGS_NAME);
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut updated_settings = false;

    if is_file(&settings_path)? {
        let mut settings = serde_json::from_str::<Value>(&fs::read_to_string(&settings_path)?)
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to parse {}: {err}",
                    settings_path.display()
                ))
            })?;

        if let Some(hooks) = hooks_object_if_present(
            &mut settings,
            &settings_path,
            "copilot settings",
            "copilot settings hooks",
        )? {
            for hook in integration_hook_events(Target::Copilot) {
                updated_settings |= remove_direct_hook_commands(
                    hooks,
                    hook.event,
                    &hook_path,
                    hook.action.map(crate::agent::IntegrationHookAction::as_str),
                )?;
            }
        }

        if updated_settings {
            write_config(&settings_path, serde_json::to_string_pretty(&settings)?)?;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Hook, hook_path, removed_hook_file);
    outcome.record_update(ArtifactRole::Settings, settings_path, updated_settings);
    Ok(outcome)
}

pub(crate) fn uninstall_devin(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let devin_dir = paths.directory(DirectoryKey::Devin)?;
    check_config_targets(&devin_dir, config_file_names(Target::Devin)?)?;
    let hook_path = devin_dir.join(DEVIN_HOOK_INSTALL_NAME);
    let settings_path = devin_dir.join(super::DEVIN_CONFIG_NAME);
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut updated_settings = false;

    if is_file(&settings_path)? {
        let mut settings = serde_json::from_str::<Value>(&fs::read_to_string(&settings_path)?)
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to parse {}: {err}",
                    settings_path.display()
                ))
            })?;

        if let Some(hooks) = hooks_object_if_present(
            &mut settings,
            &settings_path,
            "devin settings",
            "devin settings hooks",
        )? {
            for hook in integration_hook_events(Target::Devin) {
                updated_settings |= remove_hook_commands(
                    hooks,
                    hook.event,
                    &hook_path,
                    hook.action.map(crate::agent::IntegrationHookAction::as_str),
                )?;
            }
        }

        if updated_settings {
            write_config(&settings_path, serde_json::to_string_pretty(&settings)?)?;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Hook, hook_path, removed_hook_file);
    outcome.record_update(ArtifactRole::Settings, settings_path, updated_settings);
    Ok(outcome)
}

pub(crate) fn uninstall_droid(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let droid_dir = paths.directory(DirectoryKey::Droid)?;
    check_config_targets(&droid_dir, config_file_names(Target::Droid)?)?;
    let hook_path = droid_dir.join("hooks").join(DROID_HOOK_INSTALL_NAME);
    let settings_path = droid_dir.join(super::DROID_SETTINGS_NAME);
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut updated_settings = false;

    if is_file(&settings_path)? {
        let mut settings = serde_json::from_str::<Value>(&fs::read_to_string(&settings_path)?)
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to parse {}: {err}",
                    settings_path.display()
                ))
            })?;
        if let Some(hooks) = hooks_object_if_present(
            &mut settings,
            &settings_path,
            "droid settings",
            "droid settings hooks",
        )? {
            for hook in integration_hook_events(Target::Droid) {
                updated_settings |= remove_hook_commands(
                    hooks,
                    hook.event,
                    &hook_path,
                    hook.action.map(crate::agent::IntegrationHookAction::as_str),
                )?;
            }
        }

        if updated_settings {
            write_config(&settings_path, serde_json::to_string_pretty(&settings)?)?;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Hook, hook_path, removed_hook_file);
    outcome.record_update(ArtifactRole::Settings, settings_path, updated_settings);
    Ok(outcome)
}

pub(crate) fn uninstall_opencode(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let dir = paths.directory(DirectoryKey::Opencode)?;
    check_config_targets(&dir, config_file_names(Target::Opencode)?)?;
    let plugin_path = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    let tui_plugin_path = dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME);
    let mut errors = Vec::new();
    remove_cli_plugin(&dir, super::OPENCODE_V2_TUI_PLUGIN_SPEC).unwrap_or_else(|err| {
        errors.push(err.to_string());
        false
    });
    let v2_dir = dir.join(super::OPENCODE_V2_TUI_PLUGIN_DIR);
    remove_dir_all_if_exists(&v2_dir).unwrap_or_else(|err| {
        errors.push(format!("failed to remove {}: {err}", v2_dir.display()));
        false
    });
    let updated_tui_configs =
        remove_tui_plugin(&dir, OPENCODE_TUI_PLUGIN_SPEC).unwrap_or_else(|err| {
            errors.push(err.to_string());
            Vec::new()
        });
    let removed_plugin = remove_file_if_exists(&plugin_path).unwrap_or_else(|err| {
        errors.push(format!("failed to remove {}: {err}", plugin_path.display()));
        false
    });
    let removed_tui_plugin = remove_file_if_exists(&tui_plugin_path).unwrap_or_else(|err| {
        errors.push(format!(
            "failed to remove {}: {err}",
            tui_plugin_path.display()
        ));
        false
    });
    if !errors.is_empty() {
        return Err(io::Error::other(errors.join("; ")));
    }

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Plugin, plugin_path, removed_plugin);
    outcome.record_removal(ArtifactRole::TuiPlugin, tui_plugin_path, removed_tui_plugin);
    for path in updated_tui_configs {
        outcome.record(ArtifactRole::TuiConfig, path, UninstallState::Updated);
    }
    Ok(outcome)
}

pub(crate) fn uninstall_kilo(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let plugin_path = paths
        .directory(DirectoryKey::Kilo)?
        .join("plugin")
        .join(KILO_PLUGIN_INSTALL_NAME);
    let removed_plugin = remove_file_if_exists(&plugin_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Plugin, plugin_path, removed_plugin);
    Ok(outcome)
}

pub(crate) fn install_cursor(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::Cursor)?;
    check_config_targets(&dir, config_file_names(Target::Cursor)?)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "cursor config directory not found at {}. install cursor agent cli first",
            dir.display()
        )));
    }

    let hook_path = dir.join(CURSOR_HOOK_INSTALL_NAME);

    let hooks_path = dir.join(super::CURSOR_HOOKS_NAME);
    let _hooks_lock = lock_config_for_update(&hooks_path)?;
    let mut hooks_file = read_json_config(&hooks_path, json!({ "version": 1 }))?;

    if hooks_file.get("version").is_none() {
        hooks_file
            .as_object_mut()
            .ok_or_else(|| {
                io::Error::other(format!(
                    "cursor hooks file at {} must be a JSON object",
                    hooks_path.display()
                ))
            })?
            .insert("version".to_string(), json!(1));
    }

    let hooks = ensure_hooks_object(
        &mut hooks_file,
        &hooks_path,
        "cursor hooks file",
        "cursor hooks file hooks",
    )?;
    let session_command = hook_command(&hook_path, Some("session"));
    // Strip every entry carrying the command first, as the other targets do,
    // so a hand-edited one (a matcher added, say) is replaced by the canonical
    // entry that `integration status` looks for instead of being kept as-is.
    remove_simple_command_hook(hooks, "sessionStart", &session_command)?;
    ensure_simple_command_hook(hooks, "sessionStart", &session_command)?;
    let hooks_contents = serde_json::to_string_pretty(&hooks_file)?;

    write_hook_script(Target::Cursor, &hook_path)?;
    write_config(&hooks_path, hooks_contents)?;

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Hook, hook_path);
    outcome = outcome.with_artifact(ArtifactRole::UpdatedHooks, hooks_path);
    Ok(outcome)
}

pub(crate) fn uninstall_cursor(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let cursor_home = paths.directory(DirectoryKey::Cursor)?;
    check_config_targets(&cursor_home, config_file_names(Target::Cursor)?)?;
    let hook_path = cursor_home.join(CURSOR_HOOK_INSTALL_NAME);
    let hooks_path = cursor_home.join(super::CURSOR_HOOKS_NAME);
    let _hooks_lock = lock_config_for_update(&hooks_path)?;
    let mut updated_hooks = false;

    if is_file(&hooks_path)? {
        let mut hooks_file = serde_json::from_str::<Value>(&fs::read_to_string(&hooks_path)?)
            .map_err(|err| {
                io::Error::other(format!("failed to parse {}: {err}", hooks_path.display()))
            })?;

        if let Some(hooks) = hooks_object_if_present(
            &mut hooks_file,
            &hooks_path,
            "cursor hooks file",
            "cursor hooks file hooks",
        )? {
            let session_command = hook_command(&hook_path, Some("session"));
            updated_hooks |= remove_simple_command_hook(hooks, "sessionStart", &session_command)?;
        }

        if updated_hooks {
            write_config(&hooks_path, serde_json::to_string_pretty(&hooks_file)?)?;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Hook, hook_path, removed_hook_file);
    // Install canonicalizes Cursor's hooks file; uninstall only removes
    // shepr-owned entries, so its artifact role describes that narrower action.
    outcome.record_update(ArtifactRole::Hooks, hooks_path, updated_hooks);
    Ok(outcome)
}

pub(crate) fn mastracode_hook_command(hook_path: &Path, action: &str) -> String {
    hook_command(hook_path, Some(action))
}

pub(crate) fn install_mastracode(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let mastracode_home = paths.directory(DirectoryKey::Mastracode)?;
    check_config_targets(&mastracode_home, config_file_names(Target::Mastracode)?)?;
    if !is_dir(&mastracode_home)? {
        return Err(io::Error::other(format!(
            "mastracode config directory not found at {}. install mastracode first",
            mastracode_home.display()
        )));
    }
    let hook_dir = mastracode_home.join("hooks");
    let hook_path = hook_dir.join(MASTRACODE_HOOK_INSTALL_NAME);

    let hooks_path = mastracode_home.join(super::MASTRACODE_HOOKS_NAME);
    let _hooks_lock = lock_config_for_update(&hooks_path)?;
    let mut hooks_file = read_json_config(&hooks_path, json!({}))?;

    let hooks = hooks_file.as_object_mut().ok_or_else(|| {
        io::Error::other(format!(
            "mastracode hooks file at {} must be a JSON object",
            hooks_path.display()
        ))
    })?;

    // This helper writes the Mastracode-specific description, so keep its use
    // scoped to the target that owns that description.
    for hook in integration_hook_events(Target::Mastracode) {
        let Some(action) = hook.action.map(crate::agent::IntegrationHookAction::as_str) else {
            continue;
        };
        remove_flat_command_hook(hooks, hook.event, &hook_command(&hook_path, Some(action)))?;
        ensure_flat_command_hook(
            hooks,
            hook.event,
            &mastracode_hook_command(&hook_path, action),
            timeout_millis(integration_hook_timeout(Target::Mastracode)?)?,
        )?;
    }
    let hooks_contents = serde_json::to_string_pretty(&hooks_file)?;

    fs::create_dir_all(&hook_dir)?;
    write_hook_script(Target::Mastracode, &hook_path)?;
    write_config(&hooks_path, hooks_contents)?;

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Hook, hook_path);
    outcome = outcome.with_artifact(ArtifactRole::Hooks, hooks_path);
    Ok(outcome)
}

pub(crate) fn uninstall_mastracode(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let mastracode_home = paths.directory(DirectoryKey::Mastracode)?;
    check_config_targets(&mastracode_home, config_file_names(Target::Mastracode)?)?;
    let hook_path = mastracode_home
        .join("hooks")
        .join(MASTRACODE_HOOK_INSTALL_NAME);
    let hooks_path = mastracode_home.join(super::MASTRACODE_HOOKS_NAME);
    let _hooks_lock = lock_config_for_update(&hooks_path)?;
    let mut updated_hooks = false;

    if is_file(&hooks_path)? {
        let mut hooks_file = serde_json::from_str::<Value>(&fs::read_to_string(&hooks_path)?)
            .map_err(|err| {
                io::Error::other(format!("failed to parse {}: {err}", hooks_path.display()))
            })?;
        let hooks = hooks_file.as_object_mut().ok_or_else(|| {
            io::Error::other(format!(
                "mastracode hooks file at {} must be a JSON object",
                hooks_path.display()
            ))
        })?;

        for hook in integration_hook_events(Target::Mastracode) {
            let Some(action) = hook.action.map(crate::agent::IntegrationHookAction::as_str) else {
                continue;
            };
            updated_hooks |= remove_flat_command_hook(
                hooks,
                hook.event,
                &hook_command(&hook_path, Some(action)),
            )?;
        }

        if updated_hooks {
            write_config(&hooks_path, serde_json::to_string_pretty(&hooks_file)?)?;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Hook, hook_path, removed_hook_file);
    outcome.record_update(ArtifactRole::Hooks, hooks_path, updated_hooks);
    Ok(outcome)
}

pub(crate) fn install_antigravity_cli(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::AntigravityCli)?;
    check_config_targets(&dir, config_file_names(Target::AntigravityCli)?)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "antigravity cli config directory not found at {}. install antigravity cli first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(ANTIGRAVITY_CLI_HOOK_INSTALL_NAME);

    let hooks_path = dir.join(super::ANTIGRAVITY_CLI_HOOKS_NAME);
    let _hooks_lock = lock_config_for_update(&hooks_path)?;
    let mut hooks_file = read_json_config(&hooks_path, json!({}))?;

    let hooks = hooks_file.as_object_mut().ok_or_else(|| {
        io::Error::other(format!(
            "antigravity cli hooks file at {} must be a JSON object",
            hooks_path.display()
        ))
    })?;

    // The Shepr block is Shepr-owned, so rewrite it wholesale and leave every
    // other named hook untouched.
    hooks.insert(
        ANTIGRAVITY_CLI_HOOK_BLOCK_NAME.to_string(),
        antigravity_cli_hook_block(&hook_path)?,
    );
    let hooks_contents = serde_json::to_string_pretty(&hooks_file)?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::AntigravityCli, &hook_path)?;
    write_config(&hooks_path, hooks_contents)?;

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Hook, hook_path);
    outcome = outcome.with_artifact(ArtifactRole::Hooks, hooks_path);
    Ok(outcome)
}

pub(crate) fn antigravity_cli_hook_command(hook_path: &Path, action: &str) -> String {
    hook_command(hook_path, Some(action))
}

/// Builds the Shepr-owned `hooks.json` block for Antigravity CLI.
///
/// Every event Shepr registers takes a flat handler list; the `matcher`/`hooks`
/// group is only valid for the tool events, which Shepr does not use.
pub(crate) fn antigravity_cli_hook_block(hook_path: &Path) -> io::Result<Value> {
    let mut block = Map::new();
    let timeout_seconds = integration_hook_timeout(Target::AntigravityCli)?.as_secs();
    for hook in integration_hook_events(Target::AntigravityCli) {
        let Some(action) = hook.action.map(crate::agent::IntegrationHookAction::as_str) else {
            continue;
        };
        let handler = json!({
            "type": "command",
            "command": antigravity_cli_hook_command(hook_path, action),
            "timeout": timeout_seconds,
        });
        block.insert(hook.event.to_string(), json!([handler]));
    }
    Ok(Value::Object(block))
}

pub(crate) fn uninstall_antigravity_cli(
    paths: &AgentIntegrationPaths,
) -> io::Result<UninstallOutcome> {
    let dir = paths.directory(DirectoryKey::AntigravityCli)?;
    check_config_targets(&dir, config_file_names(Target::AntigravityCli)?)?;
    let hook_path = dir.join("hooks").join(ANTIGRAVITY_CLI_HOOK_INSTALL_NAME);
    let hooks_path = dir.join(super::ANTIGRAVITY_CLI_HOOKS_NAME);
    let _hooks_lock = lock_config_for_update(&hooks_path)?;
    let mut updated_hooks = false;

    if is_file(&hooks_path)? {
        let mut hooks_file = serde_json::from_str::<Value>(&fs::read_to_string(&hooks_path)?)
            .map_err(|err| {
                io::Error::other(format!("failed to parse {}: {err}", hooks_path.display()))
            })?;

        let hooks = hooks_file.as_object_mut().ok_or_else(|| {
            io::Error::other(format!(
                "antigravity cli hooks file at {} must be a JSON object",
                hooks_path.display()
            ))
        })?;

        updated_hooks = hooks.remove(ANTIGRAVITY_CLI_HOOK_BLOCK_NAME).is_some();

        if updated_hooks {
            write_config(&hooks_path, serde_json::to_string_pretty(&hooks_file)?)?;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Hook, hook_path, removed_hook_file);
    outcome.record_update(ArtifactRole::Hooks, hooks_path, updated_hooks);
    Ok(outcome)
}

/// Grok's hook asset is a POSIX `sh` script, so it runs under `sh` rather than
/// the `bash` the shared command formatter uses for the other hooks.
fn grok_hook_command(hook_path: &Path, action: Option<IntegrationHookAction>) -> String {
    hook_command_with_interpreter(hook_path, "sh", action.map(IntegrationHookAction::as_str))
}

/// The complete Shepr-owned Grok hook config, generated from its declared
/// events. Installation and status share this value so config drift is outdated.
pub(crate) fn grok_hook_config(hook_path: &Path) -> io::Result<Value> {
    let mut event_groups = BTreeMap::<&'static str, Vec<Value>>::new();
    let timeout_seconds = integration_hook_timeout(Target::Grok)?.as_secs();
    for event in integration_hook_events(Target::Grok) {
        let command = grok_hook_command(hook_path, event.action);
        let hook = json!({
            "type": "command",
            "command": command,
            "timeout": timeout_seconds,
        });
        let mut group = json!({ "hooks": [hook] });
        if let Some(matcher) = event.matcher {
            group["matcher"] = json!(matcher);
        }
        event_groups.entry(event.event).or_default().push(group);
    }
    let hooks = event_groups
        .into_iter()
        .map(|(event, groups)| (event.to_owned(), Value::Array(groups)))
        .collect::<Map<_, _>>();
    Ok(json!({
        "hooks": hooks
    }))
}

pub(crate) fn install_grok(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    let dir = paths.directory(DirectoryKey::Grok)?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "grok config directory not found at {}. install grok cli first",
            dir.display()
        )));
    }

    // Grok merges every `~/.grok/hooks/*.json`, so shepr owns a dedicated
    // config file and never edits the user's other hooks. The hook script and
    // its config live side by side under `hooks/`.
    let hooks_dir = dir.join("hooks");
    fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join(GROK_HOOK_INSTALL_NAME);
    write_hook_script(Target::Grok, &hook_path)?;

    let config_path = hooks_dir.join(super::GROK_HOOK_CONFIG_NAME);
    write_managed_asset(
        &config_path,
        serde_json::to_string_pretty(&grok_hook_config(&hook_path)?)?.as_bytes(),
        false,
    )?;

    let mut outcome = InstallOutcome::default();
    outcome = outcome.with_artifact(ArtifactRole::Hook, hook_path);
    outcome = outcome.with_artifact(ArtifactRole::HookConfig, config_path);
    Ok(outcome)
}

pub(crate) fn uninstall_grok(paths: &AgentIntegrationPaths) -> io::Result<UninstallOutcome> {
    let hooks_dir = paths.directory(DirectoryKey::Grok)?.join("hooks");
    let hook_path = hooks_dir.join(GROK_HOOK_INSTALL_NAME);
    let config_path = hooks_dir.join(super::GROK_HOOK_CONFIG_NAME);

    // shepr owns both files outright, so removal is a straight delete.
    let removed_config_file = remove_file_if_exists(&config_path)?;
    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    let mut outcome = UninstallOutcome::default();
    outcome.record_removal(ArtifactRole::Hook, hook_path, removed_hook_file);
    outcome.record_removal(ArtifactRole::HookConfig, config_path, removed_config_file);
    Ok(outcome)
}

#[cfg(test)]
mod grok_tests {
    use std::path::Path;

    use serde_json::Value;

    use super::{Target, grok_hook_command, grok_hook_config};
    use crate::agent::IntegrationHookAction;

    #[test]
    fn grok_config_uses_its_declared_hook_events() {
        let hook_path = Path::new("/home/user/grok hooks/shepr-agent-state.sh");
        let events = Target::Grok.hook_events();
        let config = grok_hook_config(hook_path).expect("test precondition");
        let configured_events = config["hooks"].as_object().expect("Grok hooks object");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "SessionStart");
        assert_eq!(events[0].action, Some(IntegrationHookAction::Session));
        for event in events {
            let groups = configured_events
                .get(event.event)
                .and_then(Value::as_array)
                .expect("declared Grok hook event");
            let command = grok_hook_command(hook_path, event.action);
            assert!(
                groups.iter().any(|group| {
                    group["matcher"].as_str() == event.matcher
                        && group["hooks"].as_array().is_some_and(|hooks| {
                            hooks.len() == 1
                                && hooks[0]["type"] == "command"
                                && hooks[0]["command"].as_str() == Some(command.as_str())
                        })
                }),
                "missing config for Grok hook event {}",
                event.event
            );
        }
    }
}
