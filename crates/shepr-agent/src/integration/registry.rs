use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::agent::IntegrationTarget as Target;

use super::command::hook_command;

pub fn integration_target_label(target: crate::agent::IntegrationTarget) -> &'static str {
    target.label()
}

#[derive(Clone, Copy)]
struct IntegrationSpec {
    target: Target,
    asset: &'static str,
    directory: &'static str,
    path: &'static [&'static str],
    version: u32,
    events: &'static [crate::agent::IntegrationHookEvent],
}

const INTEGRATION_SPECS: &[IntegrationSpec] = &[
    IntegrationSpec {
        target: Target::Pi,
        asset: super::PI_EXTENSION_ASSET,
        directory: "pi_extension",
        path: &[super::PI_EXTENSION_INSTALL_NAME],
        version: super::PI_INTEGRATION_VERSION,
        events: Target::Pi.hook_events(),
    },
    IntegrationSpec {
        target: Target::Omp,
        asset: super::OMP_EXTENSION_ASSET,
        directory: "omp_extension",
        path: &[super::OMP_EXTENSION_INSTALL_NAME],
        version: super::OMP_INTEGRATION_VERSION,
        events: Target::Omp.hook_events(),
    },
    IntegrationSpec {
        target: Target::Claude,
        asset: super::CLAUDE_HOOK_ASSET,
        directory: "claude",
        path: &["hooks", super::CLAUDE_HOOK_INSTALL_NAME],
        version: super::CLAUDE_INTEGRATION_VERSION,
        events: Target::Claude.hook_events(),
    },
    IntegrationSpec {
        target: Target::Codex,
        asset: super::CODEX_HOOK_ASSET,
        directory: "codex",
        path: &[super::CODEX_HOOK_INSTALL_NAME],
        version: super::CODEX_INTEGRATION_VERSION,
        events: Target::Codex.hook_events(),
    },
    IntegrationSpec {
        target: Target::Copilot,
        asset: super::COPILOT_HOOK_ASSET,
        directory: "copilot",
        path: &["hooks", super::COPILOT_HOOK_INSTALL_NAME],
        version: super::COPILOT_INTEGRATION_VERSION,
        events: Target::Copilot.hook_events(),
    },
    IntegrationSpec {
        target: Target::Devin,
        asset: super::DEVIN_HOOK_ASSET,
        directory: "devin",
        path: &[super::DEVIN_HOOK_INSTALL_NAME],
        version: super::DEVIN_INTEGRATION_VERSION,
        events: Target::Devin.hook_events(),
    },
    IntegrationSpec {
        target: Target::Droid,
        asset: super::DROID_HOOK_ASSET,
        directory: "droid",
        path: &["hooks", super::DROID_HOOK_INSTALL_NAME],
        version: super::DROID_INTEGRATION_VERSION,
        events: Target::Droid.hook_events(),
    },
    IntegrationSpec {
        target: Target::Kimi,
        asset: super::KIMI_HOOK_ASSET,
        directory: "kimi",
        path: &["hooks", super::KIMI_HOOK_INSTALL_NAME],
        version: super::KIMI_INTEGRATION_VERSION,
        events: Target::Kimi.hook_events(),
    },
    IntegrationSpec {
        target: Target::Opencode,
        asset: super::OPENCODE_PLUGIN_ASSET,
        directory: "opencode",
        path: &["plugins", super::OPENCODE_PLUGIN_INSTALL_NAME],
        version: super::OPENCODE_INTEGRATION_VERSION,
        events: Target::Opencode.hook_events(),
    },
    IntegrationSpec {
        target: Target::Kilo,
        asset: super::KILO_PLUGIN_ASSET,
        directory: "kilo",
        path: &["plugin", super::KILO_PLUGIN_INSTALL_NAME],
        version: super::KILO_INTEGRATION_VERSION,
        events: Target::Kilo.hook_events(),
    },
    IntegrationSpec {
        target: Target::Qodercli,
        asset: super::QODERCLI_HOOK_ASSET,
        directory: "qodercli",
        path: &["hooks", super::QODERCLI_HOOK_INSTALL_NAME],
        version: super::QODERCLI_INTEGRATION_VERSION,
        events: Target::Qodercli.hook_events(),
    },
    IntegrationSpec {
        target: Target::Qwen,
        asset: super::QWEN_HOOK_ASSET,
        directory: "qwen",
        path: &["hooks", super::QWEN_HOOK_INSTALL_NAME],
        version: super::QWEN_INTEGRATION_VERSION,
        events: Target::Qwen.hook_events(),
    },
    IntegrationSpec {
        target: Target::Cursor,
        asset: super::CURSOR_HOOK_ASSET,
        directory: "cursor",
        path: &[super::CURSOR_HOOK_INSTALL_NAME],
        version: super::CURSOR_INTEGRATION_VERSION,
        events: Target::Cursor.hook_events(),
    },
    IntegrationSpec {
        target: Target::Mastracode,
        asset: super::MASTRACODE_HOOK_ASSET,
        directory: "mastracode",
        path: &["hooks", super::MASTRACODE_HOOK_INSTALL_NAME],
        version: super::MASTRACODE_INTEGRATION_VERSION,
        events: Target::Mastracode.hook_events(),
    },
    IntegrationSpec {
        target: Target::AntigravityCli,
        asset: super::ANTIGRAVITY_CLI_HOOK_ASSET,
        directory: "antigravity_cli",
        path: &["hooks", super::ANTIGRAVITY_CLI_HOOK_INSTALL_NAME],
        version: super::ANTIGRAVITY_CLI_INTEGRATION_VERSION,
        events: Target::AntigravityCli.hook_events(),
    },
    IntegrationSpec {
        target: Target::Grok,
        asset: super::GROK_HOOK_ASSET,
        directory: "grok",
        path: &["hooks", super::GROK_HOOK_INSTALL_NAME],
        version: super::GROK_INTEGRATION_VERSION,
        events: Target::Grok.hook_events(),
    },
    IntegrationSpec {
        target: Target::Letta,
        asset: super::LETTA_HOOK_ASSET,
        directory: "letta",
        path: &["hooks", super::LETTA_HOOK_INSTALL_NAME],
        version: super::LETTA_INTEGRATION_VERSION,
        events: Target::Letta.hook_events(),
    },
];

pub(crate) fn integration_asset(target: crate::agent::IntegrationTarget) -> Option<&'static str> {
    INTEGRATION_SPECS
        .iter()
        .copied()
        .find(|spec| spec.target == target)
        .map(|spec| spec.asset)
}

fn integration_hook_events(
    target: crate::agent::IntegrationTarget,
) -> &'static [crate::agent::IntegrationHookEvent] {
    INTEGRATION_SPECS
        .iter()
        .find(|spec| spec.target == target)
        .map(|spec| spec.events)
        .unwrap_or(&[])
}

/// One row per supported target, in spec order, for `integration status`.
/// Includes `NotInstalled` rows because the command reports the full
/// supported-target inventory. A target whose directory could not be resolved
/// is an error row, so the CLI can print it instead of silently omitting it.
pub fn integration_status_rows(
    paths: &super::env::AgentIntegrationPaths,
) -> Vec<Result<super::IntegrationStatus, super::IntegrationStatusError>> {
    integration_specs(paths)
        .map(|(target, path, expected_version)| match path {
            Ok(path) => Ok(integration_status_at(target, path, expected_version)),
            Err(error) => Err(super::IntegrationStatusError {
                target,
                message: error.to_string(),
            }),
        })
        .collect()
}

/// The resolvable rows of [`integration_status_rows`]. Unresolvable targets
/// are logged and skipped: callers here only act on installed integrations.
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
                    "could not resolve integration directory while checking status"
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

pub fn print_outdated_update_notice(paths: &super::env::AgentIntegrationPaths) -> bool {
    let outdated = outdated_installed_integrations(paths);
    if outdated.is_empty() {
        return false;
    }

    let targets = outdated
        .iter()
        .map(|integration| integration.target)
        .collect::<Vec<_>>();
    eprintln!(
        "installed shepr integrations need updating; {}.",
        integration_update_instructions(&targets).replace('`', "")
    );
    true
}

/// Whether the Shepr-owned Grok hook config exactly matches the installed
/// integration. JSON formatting and object key order do not affect validity.
fn grok_hook_config_is_valid(hook_path: &Path) -> bool {
    let Some(hooks_dir) = hook_path.parent() else {
        return false;
    };
    let config_path = hooks_dir.join(super::GROK_HOOK_CONFIG_INSTALL_NAME);
    fs::read_to_string(config_path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .is_some_and(|config| config == super::targets::grok_hook_config(hook_path))
}

fn opencode_tui_integration_is_valid(plugin_path: &Path, expected_version: u32) -> bool {
    let Some(config_dir) = plugin_path.parent().and_then(Path::parent) else {
        return false;
    };
    let tui_plugin_path = config_dir.join(super::OPENCODE_TUI_PLUGIN_INSTALL_NAME);
    let tui_plugin_current = fs::read_to_string(tui_plugin_path)
        .ok()
        .and_then(|content| parse_integration_version(&content))
        .is_some_and(|version| version >= expected_version);
    tui_plugin_current
        && super::opencode_config::tui_plugin_is_configured(
            config_dir,
            super::OPENCODE_TUI_PLUGIN_SPEC,
        )
        && (!config_dir.join("cli.json").exists()
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
            .is_some_and(|version| version >= expected_version)))
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

fn json_contains_string(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value == needle,
        serde_json::Value::Array(items) => {
            items.iter().any(|item| json_contains_string(item, needle))
        }
        serde_json::Value::Object(map) => {
            map.values().any(|item| json_contains_string(item, needle))
        }
        _ => false,
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Every `(event, command)` pair appears as a command string somewhere in that
/// event's entry list. The entry shape differs per agent (nested hook groups,
/// flat entries, `bash` fields), so this searches the event's list for the
/// exact command rather than modelling each shape.
fn json_hook_commands_registered(
    config_path: &Path,
    root: HooksRoot,
    expected: &[(&str, String)],
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
            .is_some_and(|entries| json_contains_string(entries, command))
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
/// Pi, OMP and Kilo load every file in their plugin directory, so the file is
/// its own registration. Grok and opencode are checked by their own helpers.
fn hook_registration_is_current(target: crate::agent::IntegrationTarget, hook_path: &Path) -> bool {
    use crate::agent::IntegrationTarget as Target;

    let json_in = |levels: usize, file: &str, root: HooksRoot, expected: &[(&str, String)]| {
        ancestor(hook_path, levels)
            .is_some_and(|dir| json_hook_commands_registered(&dir.join(file), root, expected))
    };
    match target {
        Target::Pi | Target::Omp | Target::Kilo | Target::Grok | Target::Opencode => true,
        Target::Claude => json_in(
            2,
            "settings.json",
            HooksRoot::HooksKey,
            &hook_event_commands(
                hook_path,
                integration_hook_events(crate::agent::IntegrationTarget::Claude),
            ),
        ),
        Target::Codex => {
            json_in(
                1,
                "hooks.json",
                HooksRoot::HooksKey,
                &hook_event_commands(
                    hook_path,
                    integration_hook_events(crate::agent::IntegrationTarget::Codex),
                ),
            ) && ancestor(hook_path, 1)
                .is_some_and(|dir| codex_hooks_feature_enabled(&dir.join("config.toml")))
        }
        Target::Copilot => json_in(
            2,
            "settings.json",
            HooksRoot::HooksKey,
            &integration_hook_events(crate::agent::IntegrationTarget::Copilot)
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
        ),
        Target::Devin => json_in(
            1,
            "config.json",
            HooksRoot::HooksKey,
            &hook_event_commands(
                hook_path,
                integration_hook_events(crate::agent::IntegrationTarget::Devin),
            ),
        ),
        Target::Droid => json_in(
            2,
            "settings.json",
            HooksRoot::HooksKey,
            &hook_event_commands(
                hook_path,
                integration_hook_events(crate::agent::IntegrationTarget::Droid),
            ),
        ),
        Target::Qodercli => json_in(
            2,
            "settings.json",
            HooksRoot::HooksKey,
            &hook_event_commands(
                hook_path,
                integration_hook_events(crate::agent::IntegrationTarget::Qodercli),
            ),
        ),
        Target::Qwen => json_in(
            2,
            "settings.json",
            HooksRoot::HooksKey,
            &hook_event_commands(
                hook_path,
                integration_hook_events(crate::agent::IntegrationTarget::Qwen),
            ),
        ),
        Target::Letta => json_in(
            2,
            "settings.json",
            HooksRoot::HooksKey,
            &hook_event_commands(
                hook_path,
                integration_hook_events(crate::agent::IntegrationTarget::Letta),
            ),
        ),
        Target::Cursor => json_in(
            1,
            "hooks.json",
            HooksRoot::HooksKey,
            &hook_event_commands(
                hook_path,
                integration_hook_events(crate::agent::IntegrationTarget::Cursor),
            ),
        ),
        Target::Mastracode => json_in(
            2,
            "hooks.json",
            HooksRoot::Document,
            &hook_event_commands(
                hook_path,
                integration_hook_events(crate::agent::IntegrationTarget::Mastracode),
            ),
        ),
        Target::AntigravityCli => ancestor(hook_path, 2).is_some_and(|dir| {
            read_json(&dir.join("hooks.json")).is_some_and(|document| {
                document.get(super::ANTIGRAVITY_CLI_HOOK_BLOCK_NAME)
                    == Some(&super::targets::antigravity_cli_hook_block(hook_path))
            })
        }),
        Target::Kimi => ancestor(hook_path, 2)
            .is_some_and(|dir| kimi_hooks_registered(&dir.join("config.toml"), hook_path)),
    }
}

fn integration_state_for_path(
    path: &Path,
    expected_version: u32,
) -> (super::IntegrationStatusKind, Option<u32>) {
    if !path.is_file() {
        return (super::IntegrationStatusKind::NotInstalled, None);
    }

    let installed_version = fs::read_to_string(path)
        .ok()
        .and_then(|content| parse_integration_version(&content));
    let state = if installed_version.is_some_and(|version| version >= expected_version) {
        super::IntegrationStatusKind::Current
    } else {
        super::IntegrationStatusKind::Outdated
    };

    (state, installed_version)
}

pub(crate) fn integration_status_at(
    target: crate::agent::IntegrationTarget,
    path: PathBuf,
    expected_version: u32,
) -> super::IntegrationStatus {
    let (mut state, installed_version) = integration_state_for_path(&path, expected_version);

    // Grok only invokes the hook when the shepr-owned `hooks/shepr.json`
    // registers it, so a current hook script with a missing or broken config
    // is a nonfunctional install: report it as outdated so `shepr integration
    // status` flags it and a reinstall rewrites both files.
    if target == crate::agent::IntegrationTarget::Grok
        && state == super::IntegrationStatusKind::Current
        && !grok_hook_config_is_valid(&path)
    {
        state = super::IntegrationStatusKind::Outdated;
    }
    if target == crate::agent::IntegrationTarget::Opencode
        && state == super::IntegrationStatusKind::Current
        && !opencode_tui_integration_is_valid(&path, expected_version)
    {
        state = super::IntegrationStatusKind::Outdated;
    }
    // Every other config-registered target: a current hook script the agent's
    // config no longer (or never) points at does nothing, so it is outdated
    // too, and `shepr integration install` repairs the registration.
    if state == super::IntegrationStatusKind::Current
        && !hook_registration_is_current(target, &path)
    {
        state = super::IntegrationStatusKind::Outdated;
    }

    super::IntegrationStatus {
        target,
        path,
        state,
        installed_version,
        expected_version,
    }
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
        integration_status_at(target, hook.to_path_buf(), 1).state
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
        let installed = super::super::claude_settings::install("{}", &settings_path, &hook)
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
        let _ = fs::remove_dir_all(dir);
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
        let _ = fs::remove_dir_all(dir);
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
        let _ = fs::remove_dir_all(dir);
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
        let _ = fs::remove_dir_all(dir);
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
                .exists()
        );
        let _ = fs::remove_dir_all(claude);

        let codex = base("codex-malformed");
        fs::write(codex.join("hooks.json"), "[1,").expect("test precondition");
        env.set("CODEX_HOME", &codex);
        let result = super::super::targets::install_codex(
            &super::super::env::AgentIntegrationPaths::resolve(),
        );
        assert!(result.is_err());
        assert!(!codex.join(super::super::CODEX_HOOK_INSTALL_NAME).exists());
        let _ = fs::remove_dir_all(codex);

        let copilot = base("copilot-malformed");
        fs::write(copilot.join("settings.json"), "{\"hooks\": []}").expect("test precondition");
        env.set("COPILOT_HOME", &copilot);
        let result = super::super::targets::install_copilot(
            &super::super::env::AgentIntegrationPaths::resolve(),
        );
        assert!(result.is_err());
        assert!(!copilot.join("hooks").exists());
        let _ = fs::remove_dir_all(copilot);
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
        let _ = fs::remove_dir_all(dir);
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
        let _ = fs::remove_dir_all(dir);
    }
}
