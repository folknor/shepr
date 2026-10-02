use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::agent::{IntegrationHookAction, IntegrationTarget as Target};

use super::ANTIGRAVITY_CLI_HOOK_BLOCK_NAME;
use super::command::hook_command;
use super::config_edit::{
    build_codex_config_with_hooks, build_kimi_config_with_timeout, ensure_hooks_object,
    remove_hook_path_commands,
};
use super::config_file::{
    ConfigUpdateLock, check_config_target, lock_config_for_update, write_config_for_update,
};
use super::env::{AgentIntegrationPaths, DirectoryKey};
use super::file_ops::{read_if_file, write_managed_asset};
use super::opencode_config::{
    PluginConfigEdit, prepare_cli_plugin, prepare_tui_plugin, validate_tui_plugin_config,
};
use super::registration::{HooksRoot, JsonShape, Registration};
use super::registry::{
    action_label, agent_present, managed_assets, registration, target_directory, target_path,
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
        edit: impl FnOnce(&str, &Path) -> io::Result<String>,
    ) -> io::Result<Self> {
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

    fn write(self) -> io::Result<()> {
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
pub(super) fn install(paths: &AgentIntegrationPaths, target: Target) -> io::Result<InstallOutcome> {
    let dir = target_directory(paths, target)?;
    let registration = registration(target);
    for path in registration.config_paths(&dir) {
        check_config_target(&path)?;
    }
    if !agent_present(paths, target)? {
        return Err(missing_agent_directory(target, &dir));
    }
    if target == Target::Omp && dir == target_directory(paths, Target::Pi)? {
        return Err(io::Error::other(format!(
            "Pi and OMP resolve to the same extension directory at {}; configure separate agent directories before installing OMP",
            dir.display()
        )));
    }
    let hook_path = target_path(paths, target)?;
    let mut outcome = InstallOutcome::default();
    let mut edits = Vec::new();
    let mut plugin_edits = Vec::new();
    match registration {
        Registration::DirectoryLoaded => {}
        Registration::Json { file, root, shape } => {
            let role = match target {
                Target::Claude | Target::Copilot | Target::Devin => ArtifactRole::Settings,
                Target::Cursor => ArtifactRole::UpdatedHooks,
                _ => ArtifactRole::Hooks,
            };
            let path = dir.join(file);
            if let JsonShape::NestedClaude(timeout) = shape {
                edits.push(ConfigEdit::prepare(
                    path.clone(),
                    paths,
                    "{}",
                    |content, path| {
                        super::claude_settings::install(
                            content,
                            path,
                            &hook_path,
                            target.hook_events(),
                            timeout,
                        )
                    },
                )?);
            } else {
                edits.push(prepare_json(
                    path.clone(),
                    paths,
                    target,
                    root,
                    shape,
                    &hook_path,
                )?);
            }
            outcome = outcome.with_artifact(role, path);
        }
        Registration::Codex {
            hooks,
            config,
            timeout,
        } => {
            let hooks_path = dir.join(hooks);
            edits.push(prepare_json(
                hooks_path.clone(),
                paths,
                target,
                HooksRoot::HooksKey,
                JsonShape::Nested(timeout),
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
                .with_artifact(ArtifactRole::Hooks, hooks_path)
                .with_artifact(ArtifactRole::Config, config_path);
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
                    let mut document = parse_json(content, path)?;
                    let hooks = document.as_object_mut().ok_or_else(|| {
                        io::Error::other(format!(
                            "antigravity cli hooks file at {} must be a JSON object",
                            path.display()
                        ))
                    })?;
                    hooks.insert(
                        ANTIGRAVITY_CLI_HOOK_BLOCK_NAME.to_string(),
                        antigravity_cli_hook_block_with_timeout(&hook_path, timeout)?,
                    );
                    serde_json::to_string_pretty(&document).map_err(io::Error::other)
                },
            )?);
            outcome = outcome.with_artifact(ArtifactRole::Hooks, path);
        }
        Registration::Grok { file, timeout } => {
            let path = dir.join("hooks").join(file);
            // Grok merges every `hooks/*.json`, so this dedicated config is
            // wholly Shepr-owned: its old contents need not be valid JSON
            // (only UTF-8), but its target and lock are checked before assets.
            edits.push(ConfigEdit::prepare(path.clone(), paths, "", |_, _| {
                serde_json::to_string_pretty(&grok_hook_config_with_timeout(&hook_path, timeout)?)
                    .map_err(io::Error::other)
            })?);
            outcome = outcome.with_artifact(ArtifactRole::HookConfig, path);
        }
        Registration::Opencode => {
            validate_tui_plugin_config(&dir)?;
            let tui = prepare_tui_plugin(&dir, super::OPENCODE_TUI_PLUGIN_SPEC, paths)?;
            let cli = prepare_cli_plugin(
                &dir,
                &paths.directory(DirectoryKey::OpencodeState)?,
                super::OPENCODE_V2_TUI_PLUGIN_SPEC,
                paths,
            )?;
            plugin_edits.push((tui, true));
            if let Some(cli) = cli {
                plugin_edits.push((cli, false));
            } else {
                outcome = outcome.with_notice(
                    "OpenCode V2 is not set up yet; start opencode2 once and the next shepr server launch registers it".to_string(),
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

fn parse_json(content: &str, path: &Path) -> io::Result<Value> {
    serde_json::from_str(content)
        .map_err(|error| io::Error::other(format!("failed to parse {}: {error}", path.display())))
}

fn prepare_json(
    path: PathBuf,
    paths: &AgentIntegrationPaths,
    target: Target,
    root: HooksRoot,
    shape: JsonShape,
    hook_path: &Path,
) -> io::Result<ConfigEdit> {
    ConfigEdit::prepare(path, paths, "{}", |content, path| {
        let mut document = parse_json(content, path)?;
        if target == Target::Cursor && document.get("version").is_none() {
            document
                .as_object_mut()
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "cursor hooks file at {} must be a JSON object",
                        path.display()
                    ))
                })?
                .insert("version".to_string(), json!(1));
        }
        let hooks = match root {
            HooksRoot::HooksKey => {
                ensure_hooks_object(&mut document, path, "agent config", "agent config hooks")?
            }
            HooksRoot::Document => document.as_object_mut().ok_or_else(|| {
                io::Error::other(format!(
                    "mastracode hooks file at {} must be a JSON object",
                    path.display()
                ))
            })?,
        };
        remove_hook_path_commands(hooks, hook_path)?;
        for (event, expected) in shape.expected_events(target, hook_path)? {
            let entries = hooks
                .entry(event.clone())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| {
                    io::Error::other(format!("hook entries for {event} must be an array"))
                })?;
            if let Value::Array(expected) = expected {
                entries.extend(expected);
            }
        }
        serde_json::to_string_pretty(&document).map_err(io::Error::other)
    })
}

fn missing_agent_directory(target: Target, dir: &Path) -> io::Error {
    let (name, install_name) = match target {
        Target::Claude => ("claude", "claude code"),
        Target::Copilot => ("copilot config", "github copilot cli"),
        Target::Devin => ("devin config", "devin cli"),
        Target::Kimi => ("kimi code config", "kimi code"),
        Target::Cursor => ("cursor config", "cursor agent cli"),
        Target::AntigravityCli => ("antigravity cli config", "antigravity cli"),
        Target::Grok => ("grok config", "grok cli"),
        Target::Pi => ("pi extension", "pi"),
        Target::Omp => ("omp extension", "omp"),
        _ => {
            return io::Error::other(format!(
                "{} config directory not found at {}. install {} first",
                target.label(),
                dir.display(),
                action_label(target)
            ));
        }
    };
    io::Error::other(format!(
        "{name} directory not found at {}. install {install_name} first",
        dir.display()
    ))
}

/// Builds the Shepr-owned `hooks.json` block for Antigravity CLI.
///
/// Every event Shepr registers takes a flat handler list; the `matcher`/`hooks`
/// group is only valid for the tool events, which Shepr does not use.
pub(super) fn antigravity_cli_hook_block_with_timeout(
    hook_path: &Path,
    timeout: std::time::Duration,
) -> io::Result<Value> {
    let mut block = Map::new();
    let timeout_seconds = timeout.as_secs();
    for hook in Target::AntigravityCli.hook_events() {
        let Some(action) = hook.action.map(crate::agent::IntegrationHookAction::as_str) else {
            continue;
        };
        let handler = json!({
            "type": "command",
            "command": hook_command(hook_path, Some(action)),
            "timeout": timeout_seconds,
        });
        block.insert(hook.event.to_string(), json!([handler]));
    }
    Ok(Value::Object(block))
}

/// Grok's hook asset uses the same POSIX shell command as the other targets.
fn grok_hook_command(hook_path: &Path, action: Option<IntegrationHookAction>) -> String {
    hook_command(hook_path, action.map(IntegrationHookAction::as_str))
}

/// The complete Shepr-owned Grok hook config, generated from its declared
/// events. Installation and status share this value so config drift is outdated.
pub(super) fn grok_hook_config_with_timeout(
    hook_path: &Path,
    timeout: std::time::Duration,
) -> io::Result<Value> {
    let mut event_groups = BTreeMap::<&'static str, Vec<Value>>::new();
    let timeout_seconds = timeout.as_secs();
    for event in Target::Grok.hook_events() {
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

#[cfg(test)]
pub(crate) fn install_pi(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Pi)
}

#[cfg(test)]
pub(crate) fn install_omp(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Omp)
}

#[cfg(test)]
pub(crate) fn install_claude(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Claude)
}

#[cfg(test)]
pub(crate) fn install_codex(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Codex)
}

#[cfg(test)]
pub(crate) fn install_copilot(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Copilot)
}

#[cfg(test)]
pub(crate) fn install_devin(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Devin)
}

#[cfg(test)]
pub(crate) fn install_droid(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Droid)
}

#[cfg(test)]
pub(crate) fn install_kimi(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Kimi)
}

#[cfg(test)]
pub(crate) fn install_opencode(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Opencode)
}

#[cfg(test)]
pub(crate) fn install_kilo(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Kilo)
}

#[cfg(test)]
pub(crate) fn install_cursor(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Cursor)
}

#[cfg(test)]
pub(crate) fn install_mastracode(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Mastracode)
}

#[cfg(test)]
pub(crate) fn install_antigravity_cli(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::AntigravityCli)
}

#[cfg(test)]
pub(crate) fn install_grok(paths: &AgentIntegrationPaths) -> io::Result<InstallOutcome> {
    install(paths, Target::Grok)
}

#[cfg(test)]
pub(crate) fn grok_hook_config(hook_path: &Path) -> io::Result<Value> {
    grok_hook_config_with_timeout(hook_path, super::HOOK_TIMEOUT)
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
