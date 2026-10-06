use crate::types::{InstallError, InstallResult};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use shepr_agent::{IntegrationHookAction, IntegrationTarget as Target};

use super::ANTIGRAVITY_CLI_HOOK_BLOCK_NAME;
use super::command::hook_command;
use super::config_edit::{build_codex_config_with_hooks, build_kimi_config_with_timeout};
use super::config_file::{
    ConfigUpdateLock, check_config_target, lock_config_for_update, write_config_for_update,
};
use super::env::AgentIntegrationPaths;
use super::file_ops::{read_if_file, write_managed_asset};
use super::opencode_config::{
    PluginConfigEdit, prepare_cli_plugin, prepare_tui_plugin, validate_tui_plugin_config,
};
use super::registration::{HookEventPolicy, HooksRoot, JsonShape, Registration, RequiredJsonField};
use super::registry::{
    agent_directory, agent_present, directory_must_differ_from, managed_assets, registration,
    target_directory, target_path,
};
use super::types::{ArtifactRole, InstallOutcome};

struct ConfigEdit {
    path: PathBuf,
    lock: ConfigUpdateLock,
    contents: String,
    changed: bool,
}

impl ConfigEdit {
    fn prepare(
        path: PathBuf,
        paths: &AgentIntegrationPaths,
        default: &str,
        edit: impl FnOnce(&str, &Path) -> InstallResult<String>,
    ) -> InstallResult<Self> {
        let lock = lock_config_for_update(&path, paths)?;
        let original = read_if_file(&path)?.unwrap_or_else(|| default.to_string());
        let contents = edit(&original, &path)?;
        let changed = contents != original;
        Ok(Self {
            path,
            lock,
            contents,
            changed,
        })
    }

    fn write(self) -> InstallResult<()> {
        if self.changed {
            write_config_for_update(&self.path, &self.lock, self.contents)?;
        }
        Ok(())
    }
}

/// One publication sequence for every integration: presence and target checks,
/// acquire locks and prepare all config edits, publish assets, publish configs.
/// Preparing never creates an agent directory or publishes a hook. This is not
/// a multi-file transaction: a publication error can leave a partial install,
/// which the next status check and launch repair.
pub(super) fn install(
    paths: &AgentIntegrationPaths,
    target: Target,
) -> InstallResult<InstallOutcome> {
    let dir = target_directory(paths, target)?;
    let registration = registration(target);
    for path in registration.config_paths(&dir) {
        check_config_target(&path)?;
    }
    if !agent_present(paths, target)? {
        return Err(missing_agent_directory(
            target,
            &agent_directory(paths, target)?,
        ));
    }
    if let Some(peer) = directory_must_differ_from(target)
        && dir == target_directory(paths, peer)?
    {
        return Err(InstallError::config_shape(format!(
            "{} and {} share integration directory {}; set separate agent directories",
            peer.label(),
            target.label(),
            dir.display(),
        )));
    }
    let hook_path = target_path(paths, target)?;
    let mut outcome = InstallOutcome::default();
    let mut edits = Vec::new();
    let mut plugin_edits = Vec::new();
    match registration {
        Registration::DirectoryLoaded => {}
        Registration::Json {
            file,
            root,
            shape,
            artifact_role,
            document_description,
            required_fields,
            event_policy,
        } => {
            let path = dir.join(file);
            edits.push(prepare_json(
                path.clone(),
                paths,
                target,
                root,
                shape,
                document_description,
                required_fields,
                event_policy,
                &hook_path,
            )?);
            outcome = outcome.with_artifact(artifact_role, path);
        }
        Registration::Codex {
            hooks,
            config,
            timeout,
            hooks_artifact_role,
            config_artifact_role,
            document_description,
            required_fields,
            event_policy,
        } => {
            let hooks_path = dir.join(hooks);
            edits.push(prepare_json(
                hooks_path.clone(),
                paths,
                target,
                HooksRoot::HooksKey,
                JsonShape::Nested(timeout),
                document_description,
                required_fields,
                event_policy,
                &hook_path,
            )?);
            let config_path = dir.join(config);
            edits.push(ConfigEdit::prepare(
                config_path.clone(),
                paths,
                "",
                |content, _| build_codex_config_with_hooks(content),
            )?);
            outcome = outcome
                .with_artifact(hooks_artifact_role, hooks_path)
                .with_artifact(config_artifact_role, config_path);
        }
        Registration::Kimi { file, timeout } => {
            let path = dir.join(file);
            edits.push(ConfigEdit::prepare(
                path.clone(),
                paths,
                "",
                |content, _| build_kimi_config_with_timeout(content, &hook_path, timeout),
            )?);
            outcome = outcome.with_artifact(ArtifactRole::Config, path);
        }
        Registration::AntigravityCli { file, timeout } => {
            let path = dir.join(file);
            edits.push(ConfigEdit::prepare(
                path.clone(),
                paths,
                "{}",
                |content, path| {
                    super::json_edit::install_block(
                        content,
                        path,
                        ANTIGRAVITY_CLI_HOOK_BLOCK_NAME,
                        &antigravity_cli_hook_block_with_timeout(timeout)?,
                    )
                },
            )?);
            outcome = outcome.with_artifact(ArtifactRole::Hooks, path);
        }
        Registration::Grok { file, timeout } => {
            let path = dir.join("hooks").join(file);
            // Grok merges every `hooks/*.json`, so this dedicated config is
            // wholly Shepr-owned: its old contents need not be valid JSON
            // (only UTF-8), but its target and lock are checked before assets.
            edits.push(ConfigEdit::prepare(
                path.clone(),
                paths,
                "",
                |content, _| {
                    let expected = grok_hook_config_with_timeout(timeout)?;
                    if grok_hook_config_matches(content, &expected) {
                        return Ok(content.to_owned());
                    }
                    serde_json::to_string_pretty(&expected)
                        .map_err(|error| InstallError::from(io::Error::other(error)))
                },
            )?);
            outcome = outcome.with_artifact(ArtifactRole::HookConfig, path);
        }
        Registration::Opencode => {
            validate_tui_plugin_config(&dir)?;
            let tui = prepare_tui_plugin(&dir, super::OPENCODE_TUI_PLUGIN_SPEC, paths)?;
            let cli = prepare_cli_plugin(
                &dir,
                &paths.opencode_state_directory()?,
                super::OPENCODE_V2_TUI_PLUGIN_SPEC,
                paths,
            )?;
            plugin_edits.push((tui, true));
            if let Some(cli) = cli {
                plugin_edits.push((cli, false));
            } else {
                outcome = outcome.with_notice(
                    "OpenCode V2 is not set up yet; start the agent once and the next server launch registers it".to_string(),
                );
            }
        }
    }
    let mut installed_assets = Vec::new();
    for asset in managed_assets(target) {
        let mut path = dir.clone();
        for part in asset.path {
            path.push(part);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_managed_asset(&path, asset.contents.as_bytes(), asset.executable)?;
        // The V2 module re-exports the TUI plugin; it is not a second
        // operator-facing plugin artifact.
        if let Some(role) = asset.role {
            installed_assets.push(super::types::InstallArtifact { role, path });
        }
    }
    installed_assets.append(&mut outcome.artifacts);
    outcome.artifacts = installed_assets;
    for edit in edits {
        edit.write()?;
    }
    for (edit, is_tui) in plugin_edits {
        let path = PluginConfigEdit::write(edit)?;
        if is_tui {
            outcome = outcome.with_artifact(ArtifactRole::TuiConfig, path);
        }
    }
    Ok(outcome)
}

fn prepare_json(
    path: PathBuf,
    paths: &AgentIntegrationPaths,
    target: Target,
    root: HooksRoot,
    shape: JsonShape,
    document_description: &'static str,
    required_fields: &'static [RequiredJsonField],
    event_policy: HookEventPolicy,
    hook_path: &Path,
) -> InstallResult<ConfigEdit> {
    ConfigEdit::prepare(path, paths, "{}", |content, path| {
        super::json_edit::install_json(
            target,
            content,
            path,
            hook_path,
            root,
            shape.expected_events(target, event_policy)?,
            required_fields,
            document_description,
        )
    })
}

fn missing_agent_directory(target: Target, dir: &Path) -> super::types::InstallError {
    InstallError::agent_dir_missing(format!(
        "{} agent config directory not found at {}",
        target.label(),
        dir.display()
    ))
}

/// Builds the Shepr-owned `hooks.json` block for Antigravity CLI.
///
/// Every event Shepr registers takes a flat handler list; the `matcher`/`hooks`
/// group is only valid for the tool events, which Shepr does not use.
pub(super) fn antigravity_cli_hook_block_with_timeout(
    timeout: std::time::Duration,
) -> InstallResult<Value> {
    let mut block = Map::new();
    let timeout_seconds = timeout.as_secs();
    for hook in Target::AntigravityCli.hook_events() {
        let Some(action) = hook.action.map(shepr_agent::IntegrationHookAction::as_str) else {
            continue;
        };
        let handler = json!({
            "type": "command",
            "command": hook_command(Target::AntigravityCli, Some(action)),
            "timeout": timeout_seconds,
        });
        block.insert(hook.event.to_string(), json!([handler]));
    }
    Ok(Value::Object(block))
}

/// The complete Shepr-owned Grok hook config, generated from its declared
/// events. Installation and status share this value so config drift is outdated.
pub(super) fn grok_hook_config_with_timeout(timeout: std::time::Duration) -> InstallResult<Value> {
    let mut event_groups = BTreeMap::<&'static str, Vec<Value>>::new();
    let timeout_seconds = timeout.as_secs();
    for event in Target::Grok.hook_events() {
        let command = hook_command(
            Target::Grok,
            event.action.map(IntegrationHookAction::as_str),
        );
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

/// Use the same semantic equality for status and install's no-op decision.
pub(super) fn grok_hook_config_matches(content: &str, expected: &Value) -> bool {
    serde_json::from_str::<Value>(content).is_ok_and(|config| config == *expected)
}

#[cfg(test)]
pub(crate) fn grok_hook_config() -> InstallResult<Value> {
    grok_hook_config_with_timeout(super::HOOK_TIMEOUT)
}

#[cfg(test)]
mod grok_tests {
    use serde_json::Value;

    use super::{Target, grok_hook_config};
    use crate::command::hook_command;
    use shepr_agent::IntegrationHookAction;

    #[test]
    fn grok_config_uses_its_declared_hook_events() {
        let events = Target::Grok.hook_events();
        let config = grok_hook_config().expect("test precondition");
        let configured_events = config["hooks"].as_object().expect("Grok hooks object");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "SessionStart");
        assert_eq!(events[0].action, Some(IntegrationHookAction::Session));
        for event in events {
            let groups = configured_events
                .get(event.event)
                .and_then(Value::as_array)
                .expect("declared Grok hook event");
            let command = hook_command(
                Target::Grok,
                event.action.map(IntegrationHookAction::as_str),
            );
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
