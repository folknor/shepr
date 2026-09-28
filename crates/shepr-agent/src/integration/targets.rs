use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
use super::env::AgentIntegrationPaths;
use super::file_ops::{
    is_dir, is_file, remove_dir_all_if_exists, remove_file_if_exists, write_managed_asset,
};
use super::opencode_config::{
    PluginConfigEdit, prepare_cli_plugin, prepare_tui_plugin, remove_cli_plugin, remove_tui_plugin,
    validate_tui_plugin_config,
};
use super::types::{
    AntigravityCliInstallPaths, AntigravityCliUninstallResult, ClaudeInstallPaths,
    ClaudeUninstallResult, CodexInstallPaths, CodexUninstallResult, CopilotInstallPaths,
    CopilotUninstallResult, CursorInstallPaths, CursorUninstallResult, DevinInstallPaths,
    DevinUninstallResult, DroidInstallPaths, DroidUninstallResult, GrokInstallPaths,
    GrokUninstallResult, KiloInstallPaths, KiloUninstallResult, KimiInstallPaths,
    KimiUninstallResult, LettaInstallPaths, LettaUninstallResult, MastracodeInstallPaths,
    MastracodeUninstallResult, OmpInstallPaths, OmpUninstallResult, OpenCodeInstallPaths,
    OpenCodeUninstallResult, PiUninstallResult, QodercliInstallPaths, QodercliUninstallResult,
    QwenInstallPaths, QwenUninstallResult,
};
use super::{
    ANTIGRAVITY_CLI_HOOK_BLOCK_NAME, ANTIGRAVITY_CLI_HOOK_EVENTS,
    ANTIGRAVITY_CLI_HOOK_INSTALL_NAME, ANTIGRAVITY_CLI_HOOK_TIMEOUT_SEC, CLAUDE_HOOK_INSTALL_NAME,
    CODEX_HOOK_INSTALL_NAME, COPILOT_HOOK_EVENTS, COPILOT_HOOK_INSTALL_NAME,
    CURSOR_HOOK_INSTALL_NAME, DEVIN_HOOK_EVENTS, DEVIN_HOOK_INSTALL_NAME, DROID_HOOK_EVENTS,
    DROID_HOOK_INSTALL_NAME, GROK_HOOK_CONFIG_INSTALL_NAME, GROK_HOOK_INSTALL_NAME,
    KILO_PLUGIN_INSTALL_NAME, KIMI_HOOK_INSTALL_NAME, LETTA_HOOK_INSTALL_NAME,
    LETTA_HOOK_TIMEOUT_MS, MASTRACODE_HOOK_EVENTS, MASTRACODE_HOOK_INSTALL_NAME,
    MASTRACODE_HOOK_TIMEOUT_MS, OMP_EXTENSION_INSTALL_NAME, OPENCODE_PLUGIN_INSTALL_NAME,
    OPENCODE_TUI_PLUGIN_ASSET, OPENCODE_TUI_PLUGIN_INSTALL_NAME, OPENCODE_TUI_PLUGIN_SPEC,
    PI_EXTENSION_INSTALL_NAME, QODERCLI_HOOK_EVENTS, QODERCLI_HOOK_INSTALL_NAME, QWEN_HOOK_EVENTS,
    QWEN_HOOK_INSTALL_NAME,
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

pub(crate) fn install_pi(paths: &AgentIntegrationPaths) -> io::Result<PathBuf> {
    let dir = paths.directory("pi_extension")?;
    ensure_extension_dir(&dir, "pi")?;

    let path = dir.join(PI_EXTENSION_INSTALL_NAME);
    write_target_asset(Target::Pi, &path, false)?;
    Ok(path)
}

pub(crate) fn install_omp(paths: &AgentIntegrationPaths) -> io::Result<OmpInstallPaths> {
    let dir = paths.directory("omp_extension")?;
    let pi_dir = paths.directory("pi_extension")?;
    if dir == pi_dir {
        return Err(io::Error::other(format!(
            "Pi and OMP resolve to the same extension directory at {}; configure separate agent directories before installing OMP",
            dir.display()
        )));
    }
    ensure_extension_dir(&dir, "omp")?;

    let extension_path = dir.join(OMP_EXTENSION_INSTALL_NAME);
    write_target_asset(Target::Omp, &extension_path, false)?;
    Ok(OmpInstallPaths { extension_path })
}

pub(crate) fn install_claude(paths: &AgentIntegrationPaths) -> io::Result<ClaudeInstallPaths> {
    let dir = paths.directory("claude")?;
    check_config_targets(&dir, &["settings.json"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "claude directory not found at {}. install claude code first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);

    let settings_path = dir.join("settings.json");
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let existing_settings = if is_file(&settings_path)? {
        fs::read_to_string(&settings_path)?
    } else {
        "{}".to_string()
    };
    // Edit the settings in memory before writing anything, so settings that
    // cannot be parsed or edited leave no orphan hook behind.
    let updated_settings = install_claude_settings(&existing_settings, &settings_path, &hook_path)?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::Claude, &hook_path)?;

    if updated_settings != existing_settings {
        write_config(&settings_path, updated_settings)?;
    }

    Ok(ClaudeInstallPaths {
        hook_path,
        settings_path,
    })
}

pub(crate) fn install_codex(paths: &AgentIntegrationPaths) -> io::Result<CodexInstallPaths> {
    let dir = paths.directory("codex")?;
    check_config_targets(&dir, &["hooks.json", "config.toml"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "codex config directory not found at {}. install codex first",
            dir.display()
        )));
    }

    let hook_path = dir.join(CODEX_HOOK_INSTALL_NAME);

    let hooks_path = dir.join("hooks.json");
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
        10,
        None,
    )?;
    let hooks_contents = serde_json::to_string_pretty(&hooks_file)?;

    let config_path = dir.join("config.toml");
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

    Ok(CodexInstallPaths {
        hook_path,
        hooks_path,
        config_path,
    })
}

pub(crate) fn install_kimi(paths: &AgentIntegrationPaths) -> io::Result<KimiInstallPaths> {
    let dir = paths.directory("kimi")?;
    check_config_targets(&dir, &["config.toml"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "kimi code config directory not found at {}. install kimi code first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(KIMI_HOOK_INSTALL_NAME);
    let config_path = dir.join("config.toml");
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

    Ok(KimiInstallPaths {
        hook_path,
        config_path,
    })
}

pub(crate) fn install_copilot(paths: &AgentIntegrationPaths) -> io::Result<CopilotInstallPaths> {
    let dir = paths.directory("copilot")?;
    check_config_targets(&dir, &["settings.json"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "copilot config directory not found at {}. install github copilot cli first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(COPILOT_HOOK_INSTALL_NAME);

    let settings_path = dir.join("settings.json");
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut settings = read_json_config(&settings_path, json!({}))?;

    let hooks = ensure_hooks_object(
        &mut settings,
        &settings_path,
        "copilot settings",
        "copilot settings hooks",
    )?;
    for hook in COPILOT_HOOK_EVENTS {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        remove_direct_hook_commands(hooks, hook.event, &hook_path, action)?;
    }
    for hook in COPILOT_HOOK_EVENTS {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        ensure_direct_command_hook(
            hooks,
            hook.event,
            hook_command(&hook_path, action),
            10,
            None,
        )?;
    }
    let settings_contents = serde_json::to_string_pretty(&settings)?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::Copilot, &hook_path)?;
    write_config(&settings_path, settings_contents)?;

    Ok(CopilotInstallPaths {
        hook_path,
        settings_path,
    })
}

pub(crate) fn install_devin(paths: &AgentIntegrationPaths) -> io::Result<DevinInstallPaths> {
    let dir = paths.directory("devin")?;
    check_config_targets(&dir, &["config.json"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "devin config directory not found at {}. install devin cli first",
            dir.display()
        )));
    }

    let hook_path = dir.join(DEVIN_HOOK_INSTALL_NAME);

    let settings_path = dir.join("config.json");
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut settings = read_json_config(&settings_path, json!({}))?;

    let hooks = ensure_hooks_object(
        &mut settings,
        &settings_path,
        "devin settings",
        "devin settings hooks",
    )?;
    for hook in DEVIN_HOOK_EVENTS {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        remove_hook_commands(hooks, hook.event, &hook_path, action)?;
    }
    for hook in DEVIN_HOOK_EVENTS {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        ensure_command_hook(
            hooks,
            hook.event,
            &hook_command(&hook_path, action),
            10,
            None,
        )?;
    }
    let settings_contents = serde_json::to_string_pretty(&settings)?;

    write_hook_script(Target::Devin, &hook_path)?;
    write_config(&settings_path, settings_contents)?;

    Ok(DevinInstallPaths {
        hook_path,
        settings_path,
    })
}

pub(crate) fn install_droid(paths: &AgentIntegrationPaths) -> io::Result<DroidInstallPaths> {
    let dir = paths.directory("droid")?;
    check_config_targets(&dir, &["settings.json"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "droid config directory not found at {}. install droid first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(DROID_HOOK_INSTALL_NAME);

    let settings_path = dir.join("settings.json");
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut settings = read_json_config(&settings_path, json!({}))?;

    let hooks = ensure_hooks_object(
        &mut settings,
        &settings_path,
        "droid settings",
        "droid settings hooks",
    )?;
    for hook in DROID_HOOK_EVENTS {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        remove_hook_commands(hooks, hook.event, &hook_path, action)?;
    }
    for hook in DROID_HOOK_EVENTS {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        ensure_command_hook(
            hooks,
            hook.event,
            &hook_command(&hook_path, action),
            10,
            None,
        )?;
    }
    let settings_contents = serde_json::to_string_pretty(&settings)?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::Droid, &hook_path)?;
    write_config(&settings_path, settings_contents)?;

    Ok(DroidInstallPaths {
        hook_path,
        settings_path,
    })
}

pub(crate) fn install_opencode(paths: &AgentIntegrationPaths) -> io::Result<OpenCodeInstallPaths> {
    let dir = paths.directory("opencode")?;
    check_config_targets(&dir, &["tui.jsonc", "tui.json", "cli.json"])?;
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
        &paths.directory("opencode_state")?,
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

    Ok(OpenCodeInstallPaths {
        plugin_path,
        tui_plugin_path,
        tui_config_path,
        cli_config_path,
    })
}

pub(crate) fn install_kilo(paths: &AgentIntegrationPaths) -> io::Result<KiloInstallPaths> {
    let dir = paths.directory("kilo")?;
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

    Ok(KiloInstallPaths { plugin_path })
}

pub(crate) fn uninstall_pi(paths: &AgentIntegrationPaths) -> io::Result<PiUninstallResult> {
    let extension_path = paths
        .directory("pi_extension")?
        .join(PI_EXTENSION_INSTALL_NAME);
    let removed_extension = remove_file_if_exists(&extension_path)?;

    Ok(PiUninstallResult {
        extension_path,
        removed_extension,
    })
}

pub(crate) fn uninstall_omp(paths: &AgentIntegrationPaths) -> io::Result<OmpUninstallResult> {
    let extension_path = paths
        .directory("omp_extension")?
        .join(OMP_EXTENSION_INSTALL_NAME);
    let removed_extension = remove_file_if_exists(&extension_path)?;

    Ok(OmpUninstallResult {
        extension_path,
        removed_extension,
    })
}

pub(crate) fn uninstall_claude(paths: &AgentIntegrationPaths) -> io::Result<ClaudeUninstallResult> {
    let dir = paths.directory("claude")?;
    check_config_targets(&dir, &["settings.json"])?;
    let hook_path = dir.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME);
    let settings_path = dir.join("settings.json");
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut updated_settings = false;

    if is_file(&settings_path)? {
        let existing_settings = fs::read_to_string(&settings_path)?;
        let new_settings =
            uninstall_claude_settings(&existing_settings, &settings_path, &hook_path)?;
        updated_settings = new_settings != existing_settings;
        if updated_settings {
            write_config(&settings_path, new_settings)?;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    Ok(ClaudeUninstallResult {
        hook_path,
        settings_path,
        removed_hook_file,
        updated_settings,
    })
}

pub(crate) fn uninstall_codex(paths: &AgentIntegrationPaths) -> io::Result<CodexUninstallResult> {
    let codex_dir = paths.directory("codex")?;
    check_config_targets(&codex_dir, &["hooks.json"])?;
    let hook_path = codex_dir.join(CODEX_HOOK_INSTALL_NAME);
    let hooks_path = codex_dir.join("hooks.json");
    let config_path = codex_dir.join("config.toml");
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

    Ok(CodexUninstallResult {
        hook_path,
        hooks_path,
        config_path,
        removed_hook_file,
        updated_hooks,
    })
}

pub(crate) fn uninstall_kimi(paths: &AgentIntegrationPaths) -> io::Result<KimiUninstallResult> {
    let kimi_dir = paths.directory("kimi")?;
    check_config_targets(&kimi_dir, &["config.toml"])?;
    let hook_path = kimi_dir.join("hooks").join(KIMI_HOOK_INSTALL_NAME);
    let config_path = kimi_dir.join("config.toml");
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

    Ok(KimiUninstallResult {
        hook_path,
        config_path,
        removed_hook_file,
        updated_config,
    })
}

pub(crate) fn uninstall_copilot(
    paths: &AgentIntegrationPaths,
) -> io::Result<CopilotUninstallResult> {
    let copilot_dir = paths.directory("copilot")?;
    check_config_targets(&copilot_dir, &["settings.json"])?;
    let hook_path = copilot_dir.join("hooks").join(COPILOT_HOOK_INSTALL_NAME);
    let settings_path = copilot_dir.join("settings.json");
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
            for hook in COPILOT_HOOK_EVENTS {
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

    Ok(CopilotUninstallResult {
        hook_path,
        settings_path,
        removed_hook_file,
        updated_settings,
    })
}

pub(crate) fn uninstall_devin(paths: &AgentIntegrationPaths) -> io::Result<DevinUninstallResult> {
    let devin_dir = paths.directory("devin")?;
    check_config_targets(&devin_dir, &["config.json"])?;
    let hook_path = devin_dir.join(DEVIN_HOOK_INSTALL_NAME);
    let settings_path = devin_dir.join("config.json");
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
            for hook in DEVIN_HOOK_EVENTS {
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

    Ok(DevinUninstallResult {
        hook_path,
        settings_path,
        removed_hook_file,
        updated_settings,
    })
}

pub(crate) fn uninstall_droid(paths: &AgentIntegrationPaths) -> io::Result<DroidUninstallResult> {
    let droid_dir = paths.directory("droid")?;
    check_config_targets(&droid_dir, &["settings.json"])?;
    let hook_path = droid_dir.join("hooks").join(DROID_HOOK_INSTALL_NAME);
    let settings_path = droid_dir.join("settings.json");
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
            for hook in DROID_HOOK_EVENTS {
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

    Ok(DroidUninstallResult {
        hook_path,
        settings_path,
        removed_hook_file,
        updated_settings,
    })
}

pub(crate) fn uninstall_opencode(
    paths: &AgentIntegrationPaths,
) -> io::Result<OpenCodeUninstallResult> {
    let dir = paths.directory("opencode")?;
    check_config_targets(&dir, &["tui.jsonc", "tui.json", "cli.json"])?;
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

    Ok(OpenCodeUninstallResult {
        plugin_path,
        tui_plugin_path,
        removed_plugin,
        removed_tui_plugin,
        updated_tui_configs,
    })
}

pub(crate) fn uninstall_kilo(paths: &AgentIntegrationPaths) -> io::Result<KiloUninstallResult> {
    let plugin_path = paths
        .directory("kilo")?
        .join("plugin")
        .join(KILO_PLUGIN_INSTALL_NAME);
    let removed_plugin = remove_file_if_exists(&plugin_path)?;

    Ok(KiloUninstallResult {
        plugin_path,
        removed_plugin,
    })
}

pub(crate) fn install_qodercli(paths: &AgentIntegrationPaths) -> io::Result<QodercliInstallPaths> {
    let dir = paths.directory("qodercli")?;
    check_config_targets(&dir, &["settings.json"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "qodercli config directory not found at {}. install qodercli first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(QODERCLI_HOOK_INSTALL_NAME);

    // Register the hook in ~/.qoder/settings.json. The schema mirrors claude
    // settings.json (per https://docs.qoder.com/zh/cli/hooks): a top-level
    // `hooks` object keyed by event name, each entry holding a matcher + a
    // list of `{type: "command", command, timeout?}` invocations. The hook
    // script reads the event payload from stdin via `hook_event_name`.
    let settings_path = dir.join("settings.json");
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut settings = read_json_config(&settings_path, json!({}))?;

    let hooks = ensure_hooks_object(
        &mut settings,
        &settings_path,
        "qodercli settings",
        "qodercli settings hooks",
    )?;
    for hook in QODERCLI_HOOK_EVENTS {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        remove_hook_commands(hooks, hook.event, &hook_path, action)?;
    }
    for hook in QODERCLI_HOOK_EVENTS {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        ensure_command_hook(
            hooks,
            hook.event,
            &hook_command(&hook_path, action),
            10,
            Some("*"),
        )?;
    }
    let settings_contents = serde_json::to_string_pretty(&settings)?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::Qodercli, &hook_path)?;
    write_config(&settings_path, settings_contents)?;

    Ok(QodercliInstallPaths {
        hook_path,
        settings_path,
    })
}

pub(crate) fn install_qwen(paths: &AgentIntegrationPaths) -> io::Result<QwenInstallPaths> {
    let dir = paths.directory("qwen")?;
    check_config_targets(&dir, &["settings.json"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "qwen code config directory not found at {}. install qwen code first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(QWEN_HOOK_INSTALL_NAME);

    let settings_path = dir.join("settings.json");
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut settings = read_json_config(&settings_path, json!({}))?;

    let hooks = ensure_hooks_object(
        &mut settings,
        &settings_path,
        "qwen settings",
        "qwen settings hooks",
    )?;
    for hook in QWEN_HOOK_EVENTS {
        let action = hook.action.map(crate::agent::IntegrationHookAction::as_str);
        remove_hook_commands(hooks, hook.event, &hook_path, action)?;
        ensure_command_hook(
            hooks,
            hook.event,
            &hook_command(&hook_path, action),
            10_000,
            Some("*"),
        )?;
    }
    let settings_contents = serde_json::to_string_pretty(&settings)?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::Qwen, &hook_path)?;
    write_config(&settings_path, settings_contents)?;

    Ok(QwenInstallPaths {
        hook_path,
        settings_path,
    })
}

/// Put a managed hook script back the way it was before a failed install:
/// rewrite its previous contents, or remove it if the install created it.
fn restore_letta_hook(hook_path: &Path, previous: Option<&[u8]>) -> io::Result<()> {
    match previous {
        Some(contents) => write_managed_asset(hook_path, contents, true),
        None => remove_file_if_exists(hook_path).map(|_| ()),
    }
}

fn ensure_letta_session_hook(hooks: &mut Map<String, Value>, command: &str) -> io::Result<()> {
    let entries = hooks
        .entry("SessionStart".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| io::Error::other("hook entries for SessionStart must be an array"))?;

    entries.push(json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": LETTA_HOOK_TIMEOUT_MS,
            "quiet": true,
        }],
    }));
    Ok(())
}

pub(crate) fn install_letta(paths: &AgentIntegrationPaths) -> io::Result<LettaInstallPaths> {
    let dir = paths.directory("letta")?;
    check_config_targets(&dir, &["settings.json"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "letta code config directory not found at {}. install letta code first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(LETTA_HOOK_INSTALL_NAME);

    let settings_path = dir.join("settings.json");
    let _settings_lock = lock_config_for_update(&settings_path)?;
    let mut settings = if is_file(&settings_path)? {
        serde_json::from_str::<Value>(&fs::read_to_string(&settings_path)?).map_err(|err| {
            io::Error::other(format!(
                "failed to parse {}: {err}",
                settings_path.display()
            ))
        })?
    } else {
        json!({})
    };

    let hooks = ensure_hooks_object(
        &mut settings,
        &settings_path,
        "letta settings",
        "letta settings hooks",
    )?;
    remove_hook_commands(hooks, "SessionStart", &hook_path, Some("session"))?;
    ensure_letta_session_hook(hooks, &hook_command(&hook_path, Some("session")))?;

    // Settings are parsed and edited before anything is written, so a
    // malformed settings file leaves no hook behind. The settings file is
    // user-owned config and goes through the protected writer (hard-link
    // rejection, symlink targets kept, permissions preserved, atomic replace);
    // the hook script is a managed asset and is written like every other one.
    let settings_contents = serde_json::to_string_pretty(&settings)?;
    fs::create_dir_all(&hooks_dir)?;
    let previous_hook = match fs::read(&hook_path) {
        Ok(contents) => Some(contents),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };
    let result = write_hook_script(Target::Letta, &hook_path)
        .and_then(|()| write_config(&settings_path, &settings_contents));
    if let Err(err) = result {
        return Err(
            match restore_letta_hook(&hook_path, previous_hook.as_deref()) {
                Ok(()) => err,
                Err(rollback_err) => io::Error::new(
                    err.kind(),
                    format!(
                        "{err}; restoring {} failed: {rollback_err}",
                        hook_path.display()
                    ),
                ),
            },
        );
    }

    Ok(LettaInstallPaths {
        hook_path,
        settings_path,
    })
}

pub(crate) fn install_cursor(paths: &AgentIntegrationPaths) -> io::Result<CursorInstallPaths> {
    let dir = paths.directory("cursor")?;
    check_config_targets(&dir, &["hooks.json"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "cursor config directory not found at {}. install cursor agent cli first",
            dir.display()
        )));
    }

    let hook_path = dir.join(CURSOR_HOOK_INSTALL_NAME);

    let hooks_path = dir.join("hooks.json");
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

    Ok(CursorInstallPaths {
        hook_path,
        hooks_path,
    })
}

pub(crate) fn uninstall_qodercli(
    paths: &AgentIntegrationPaths,
) -> io::Result<QodercliUninstallResult> {
    let dir = paths.directory("qodercli")?;
    check_config_targets(&dir, &["settings.json"])?;
    let hook_path = dir.join("hooks").join(QODERCLI_HOOK_INSTALL_NAME);
    let settings_path = dir.join("settings.json");
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
            "qodercli settings",
            "qodercli settings hooks",
        )? {
            for hook in QODERCLI_HOOK_EVENTS {
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

    Ok(QodercliUninstallResult {
        hook_path,
        settings_path,
        removed_hook_file,
        updated_settings,
    })
}

pub(crate) fn uninstall_qwen(paths: &AgentIntegrationPaths) -> io::Result<QwenUninstallResult> {
    let dir = paths.directory("qwen")?;
    check_config_targets(&dir, &["settings.json"])?;
    let hook_path = dir.join("hooks").join(QWEN_HOOK_INSTALL_NAME);
    let settings_path = dir.join("settings.json");
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
            "qwen settings",
            "qwen settings hooks",
        )? {
            for hook in QWEN_HOOK_EVENTS {
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

    Ok(QwenUninstallResult {
        hook_path,
        settings_path,
        removed_hook_file,
        updated_settings,
    })
}

pub(crate) fn uninstall_letta(paths: &AgentIntegrationPaths) -> io::Result<LettaUninstallResult> {
    let dir = paths.directory("letta")?;
    check_config_targets(&dir, &["settings.json"])?;
    let hook_path = dir.join("hooks").join(LETTA_HOOK_INSTALL_NAME);
    let settings_path = dir.join("settings.json");
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
            "letta settings",
            "letta settings hooks",
        )? {
            updated_settings |=
                remove_hook_commands(hooks, "SessionStart", &hook_path, Some("session"))?;
        }

        if updated_settings {
            write_config(&settings_path, serde_json::to_string_pretty(&settings)?)?;
        }
    }

    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    Ok(LettaUninstallResult {
        hook_path,
        settings_path,
        removed_hook_file,
        updated_settings,
    })
}

pub(crate) fn uninstall_cursor(paths: &AgentIntegrationPaths) -> io::Result<CursorUninstallResult> {
    let cursor_home = paths.directory("cursor")?;
    check_config_targets(&cursor_home, &["hooks.json"])?;
    let hook_path = cursor_home.join(CURSOR_HOOK_INSTALL_NAME);
    let hooks_path = cursor_home.join("hooks.json");
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

    Ok(CursorUninstallResult {
        hook_path,
        hooks_path,
        removed_hook_file,
        updated_hooks,
    })
}

pub(crate) fn mastracode_hook_command(hook_path: &Path, action: &str) -> String {
    hook_command(hook_path, Some(action))
}

pub(crate) fn install_mastracode(
    paths: &AgentIntegrationPaths,
) -> io::Result<MastracodeInstallPaths> {
    let mastracode_home = paths.directory("mastracode")?;
    check_config_targets(&mastracode_home, &["hooks.json"])?;
    if !is_dir(&mastracode_home)? {
        return Err(io::Error::other(format!(
            "mastracode config directory not found at {}. install mastracode first",
            mastracode_home.display()
        )));
    }
    let hook_dir = mastracode_home.join("hooks");
    let hook_path = hook_dir.join(MASTRACODE_HOOK_INSTALL_NAME);

    let hooks_path = mastracode_home.join("hooks.json");
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
    for hook in MASTRACODE_HOOK_EVENTS {
        let Some(action) = hook.action.map(crate::agent::IntegrationHookAction::as_str) else {
            continue;
        };
        remove_flat_command_hook(hooks, hook.event, &hook_command(&hook_path, Some(action)))?;
        ensure_flat_command_hook(
            hooks,
            hook.event,
            &mastracode_hook_command(&hook_path, action),
            MASTRACODE_HOOK_TIMEOUT_MS,
        )?;
    }
    let hooks_contents = serde_json::to_string_pretty(&hooks_file)?;

    fs::create_dir_all(&hook_dir)?;
    write_hook_script(Target::Mastracode, &hook_path)?;
    write_config(&hooks_path, hooks_contents)?;

    Ok(MastracodeInstallPaths {
        hook_path,
        hooks_path,
    })
}

pub(crate) fn uninstall_mastracode(
    paths: &AgentIntegrationPaths,
) -> io::Result<MastracodeUninstallResult> {
    let mastracode_home = paths.directory("mastracode")?;
    check_config_targets(&mastracode_home, &["hooks.json"])?;
    let hook_path = mastracode_home
        .join("hooks")
        .join(MASTRACODE_HOOK_INSTALL_NAME);
    let hooks_path = mastracode_home.join("hooks.json");
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

        for hook in MASTRACODE_HOOK_EVENTS {
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

    Ok(MastracodeUninstallResult {
        hook_path,
        hooks_path,
        removed_hook_file,
        updated_hooks,
    })
}

pub(crate) fn install_antigravity_cli(
    paths: &AgentIntegrationPaths,
) -> io::Result<AntigravityCliInstallPaths> {
    let dir = paths.directory("antigravity_cli")?;
    check_config_targets(&dir, &["hooks.json"])?;
    if !is_dir(&dir)? {
        return Err(io::Error::other(format!(
            "antigravity cli config directory not found at {}. install antigravity cli first",
            dir.display()
        )));
    }

    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(ANTIGRAVITY_CLI_HOOK_INSTALL_NAME);

    let hooks_path = dir.join("hooks.json");
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
        antigravity_cli_hook_block(&hook_path),
    );
    let hooks_contents = serde_json::to_string_pretty(&hooks_file)?;

    fs::create_dir_all(&hooks_dir)?;
    write_hook_script(Target::AntigravityCli, &hook_path)?;
    write_config(&hooks_path, hooks_contents)?;

    Ok(AntigravityCliInstallPaths {
        hook_path,
        hooks_path,
    })
}

pub(crate) fn antigravity_cli_hook_command(hook_path: &Path, action: &str) -> String {
    hook_command(hook_path, Some(action))
}

/// Builds the Shepr-owned `hooks.json` block for Antigravity CLI.
///
/// Every event Shepr registers takes a flat handler list; the `matcher`/`hooks`
/// group is only valid for the tool events, which Shepr does not use.
pub(crate) fn antigravity_cli_hook_block(hook_path: &Path) -> Value {
    let mut block = Map::new();
    for hook in ANTIGRAVITY_CLI_HOOK_EVENTS {
        let Some(action) = hook.action.map(crate::agent::IntegrationHookAction::as_str) else {
            continue;
        };
        let handler = json!({
            "type": "command",
            "command": antigravity_cli_hook_command(hook_path, action),
            "timeout": ANTIGRAVITY_CLI_HOOK_TIMEOUT_SEC,
        });
        block.insert(hook.event.to_string(), json!([handler]));
    }
    Value::Object(block)
}

pub(crate) fn uninstall_antigravity_cli(
    paths: &AgentIntegrationPaths,
) -> io::Result<AntigravityCliUninstallResult> {
    let dir = paths.directory("antigravity_cli")?;
    check_config_targets(&dir, &["hooks.json"])?;
    let hook_path = dir.join("hooks").join(ANTIGRAVITY_CLI_HOOK_INSTALL_NAME);
    let hooks_path = dir.join("hooks.json");
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

    Ok(AntigravityCliUninstallResult {
        hook_path,
        hooks_path,
        removed_hook_file,
        updated_hooks,
    })
}

/// Grok's bundled hook uses `/bin/sh`, so its configured command uses `sh` too.
fn grok_hook_command(hook_path: &Path, action: Option<IntegrationHookAction>) -> String {
    hook_command_with_interpreter(hook_path, "sh", action.map(IntegrationHookAction::as_str))
}

/// The complete Shepr-owned Grok hook config, generated from its declared
/// events. Installation and status share this value so config drift is outdated.
pub(crate) fn grok_hook_config(hook_path: &Path) -> Value {
    let mut event_groups = BTreeMap::<&'static str, Vec<Value>>::new();
    for event in Target::Grok.hook_events() {
        let command = grok_hook_command(hook_path, event.action);
        let hook = json!({
            "type": "command",
            "command": command,
            "timeout": 10,
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
    json!({
        "hooks": hooks
    })
}

pub(crate) fn install_grok(paths: &AgentIntegrationPaths) -> io::Result<GrokInstallPaths> {
    let dir = paths.directory("grok")?;
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

    let config_path = hooks_dir.join(GROK_HOOK_CONFIG_INSTALL_NAME);
    write_managed_asset(
        &config_path,
        serde_json::to_string_pretty(&grok_hook_config(&hook_path))?.as_bytes(),
        false,
    )?;

    Ok(GrokInstallPaths {
        hook_path,
        config_path,
    })
}

pub(crate) fn uninstall_grok(paths: &AgentIntegrationPaths) -> io::Result<GrokUninstallResult> {
    let hooks_dir = paths.directory("grok")?.join("hooks");
    let hook_path = hooks_dir.join(GROK_HOOK_INSTALL_NAME);
    let config_path = hooks_dir.join(GROK_HOOK_CONFIG_INSTALL_NAME);

    // shepr owns both files outright, so removal is a straight delete.
    let removed_config_file = remove_file_if_exists(&config_path)?;
    let removed_hook_file = remove_file_if_exists(&hook_path)?;

    Ok(GrokUninstallResult {
        hook_path,
        config_path,
        removed_hook_file,
        removed_config_file,
    })
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
        let config = grok_hook_config(hook_path);
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
