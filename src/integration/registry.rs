use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::command::hook_command;
use super::env::*;

pub(crate) fn integration_target_label(
    target: crate::api::schema::IntegrationTarget,
) -> &'static str {
    match target {
        crate::api::schema::IntegrationTarget::Pi => "pi",
        crate::api::schema::IntegrationTarget::Omp => "omp",
        crate::api::schema::IntegrationTarget::Claude => "claude",
        crate::api::schema::IntegrationTarget::Codex => "codex",
        crate::api::schema::IntegrationTarget::Copilot => "copilot",
        crate::api::schema::IntegrationTarget::Devin => "devin",
        crate::api::schema::IntegrationTarget::Droid => "droid",
        crate::api::schema::IntegrationTarget::Kimi => "kimi",
        crate::api::schema::IntegrationTarget::Opencode => "opencode",
        crate::api::schema::IntegrationTarget::Kilo => "kilo",
        crate::api::schema::IntegrationTarget::Hermes => "hermes",
        crate::api::schema::IntegrationTarget::Qodercli => "qodercli",
        crate::api::schema::IntegrationTarget::Qwen => "qwen",
        crate::api::schema::IntegrationTarget::Cursor => "cursor",
        crate::api::schema::IntegrationTarget::Mastracode => "mastracode",
        crate::api::schema::IntegrationTarget::AntigravityCli => "antigravity-cli",
        crate::api::schema::IntegrationTarget::Grok => "grok",
    }
}

pub(crate) fn installed_integration_statuses() -> Vec<super::IntegrationStatus> {
    integration_specs()
        .into_iter()
        .filter_map(|(target, path, expected_version)| {
            Some(integration_status_at(target, path.ok()?, expected_version))
        })
        .collect()
}

pub(crate) fn outdated_installed_integrations() -> Vec<super::IntegrationStatus> {
    installed_integration_statuses()
        .into_iter()
        .filter(|status| status.state == super::IntegrationStatusKind::Outdated)
        .collect()
}

fn integration_specs() -> [(
    crate::api::schema::IntegrationTarget,
    io::Result<PathBuf>,
    u32,
); 17] {
    [
        (
            crate::api::schema::IntegrationTarget::Pi,
            pi_extension_dir().map(|dir| dir.join(super::PI_EXTENSION_INSTALL_NAME)),
            super::PI_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Omp,
            omp_extension_dir().map(|dir| dir.join(super::OMP_EXTENSION_INSTALL_NAME)),
            super::OMP_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Claude,
            claude_dir().map(|dir| dir.join("hooks").join(super::CLAUDE_HOOK_INSTALL_NAME)),
            super::CLAUDE_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Codex,
            codex_dir().map(|dir| dir.join(super::CODEX_HOOK_INSTALL_NAME)),
            super::CODEX_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Copilot,
            copilot_dir().map(|dir| dir.join("hooks").join(super::COPILOT_HOOK_INSTALL_NAME)),
            super::COPILOT_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Devin,
            devin_dir().map(|dir| dir.join(super::DEVIN_HOOK_INSTALL_NAME)),
            super::DEVIN_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Droid,
            droid_dir().map(|dir| dir.join("hooks").join(super::DROID_HOOK_INSTALL_NAME)),
            super::DROID_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Kimi,
            kimi_dir().map(|dir| dir.join("hooks").join(super::KIMI_HOOK_INSTALL_NAME)),
            super::KIMI_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Opencode,
            opencode_dir().map(|dir| {
                dir.join("plugins")
                    .join(super::OPENCODE_PLUGIN_INSTALL_NAME)
            }),
            super::OPENCODE_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Kilo,
            kilo_dir().map(|dir| dir.join("plugin").join(super::KILO_PLUGIN_INSTALL_NAME)),
            super::KILO_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Hermes,
            hermes_plugin_dir().map(|dir| dir.join(super::HERMES_PLUGIN_INIT_INSTALL_NAME)),
            super::HERMES_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Qodercli,
            qodercli_dir().map(|dir| dir.join("hooks").join(super::QODERCLI_HOOK_INSTALL_NAME)),
            super::QODERCLI_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Qwen,
            qwen_dir().map(|dir| dir.join("hooks").join(super::QWEN_HOOK_INSTALL_NAME)),
            super::QWEN_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Cursor,
            cursor_dir().map(|dir| dir.join(super::CURSOR_HOOK_INSTALL_NAME)),
            super::CURSOR_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Mastracode,
            mastracode_dir().map(|dir| dir.join("hooks").join(super::MASTRACODE_HOOK_INSTALL_NAME)),
            super::MASTRACODE_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::AntigravityCli,
            antigravity_cli_dir().map(|dir| {
                dir.join("hooks")
                    .join(super::ANTIGRAVITY_CLI_HOOK_INSTALL_NAME)
            }),
            super::ANTIGRAVITY_CLI_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Grok,
            grok_dir().map(|dir| dir.join("hooks").join(super::GROK_HOOK_INSTALL_NAME)),
            super::GROK_INTEGRATION_VERSION,
        ),
    ]
}

pub(crate) fn integration_update_instructions(
    targets: &[crate::api::schema::IntegrationTarget],
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

pub(crate) fn print_outdated_update_notice() -> bool {
    let outdated = outdated_installed_integrations();
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
    super::KIMI_HOOK_EVENTS
        .iter()
        .all(|(event, matcher, action)| {
            let command = hook_command(hook_path, Some(*action));
            entries.iter().any(|entry| {
                entry.get("event").and_then(toml::Value::as_str) == Some(*event)
                    && entry.get("command").and_then(toml::Value::as_str) == Some(command.as_str())
                    && entry.get("matcher").and_then(toml::Value::as_str) == *matcher
            })
        })
}

/// `(event, action)` pairs to the `(event, command)` pairs install registers.
fn hook_event_commands<'a>(hook_path: &Path, events: &[(&'a str, &str)]) -> Vec<(&'a str, String)> {
    events
        .iter()
        .map(|&(event, action)| (event, hook_command(hook_path, Some(action))))
        .collect()
}

/// Whether the agent's own config still registers the installed hook, the
/// way install wrote it. The hook file's version marker alone cannot tell: an
/// install whose config edit failed, or a user who deleted the settings entry,
/// leaves a current hook script the agent never runs.
///
/// Pi, OMP and Kilo load every file in their plugin directory, so the file is
/// its own registration. Grok and opencode are checked by their own helpers.
fn hook_registration_is_current(
    target: crate::api::schema::IntegrationTarget,
    hook_path: &Path,
) -> bool {
    use crate::api::schema::IntegrationTarget as Target;

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
            &[("SessionStart", hook_command(hook_path, Some("session")))],
        ),
        Target::Codex => {
            json_in(
                1,
                "hooks.json",
                HooksRoot::HooksKey,
                &[("SessionStart", hook_command(hook_path, Some("session")))],
            ) && ancestor(hook_path, 1)
                .is_some_and(|dir| codex_hooks_feature_enabled(&dir.join("config.toml")))
        }
        Target::Copilot => json_in(
            2,
            "settings.json",
            HooksRoot::HooksKey,
            &super::COPILOT_HOOK_EVENTS
                .iter()
                .map(|&event| (event, hook_command(hook_path, None)))
                .collect::<Vec<_>>(),
        ),
        Target::Devin => json_in(
            1,
            "config.json",
            HooksRoot::HooksKey,
            &hook_event_commands(hook_path, &super::DEVIN_HOOK_EVENTS),
        ),
        Target::Droid => json_in(
            2,
            "settings.json",
            HooksRoot::HooksKey,
            &hook_event_commands(hook_path, &super::DROID_HOOK_EVENTS),
        ),
        Target::Qodercli => json_in(
            2,
            "settings.json",
            HooksRoot::HooksKey,
            &hook_event_commands(hook_path, &super::QODERCLI_HOOK_EVENTS),
        ),
        Target::Qwen => json_in(
            2,
            "settings.json",
            HooksRoot::HooksKey,
            &hook_event_commands(hook_path, &super::QWEN_HOOK_EVENTS),
        ),
        Target::Cursor => json_in(
            1,
            "hooks.json",
            HooksRoot::HooksKey,
            &[("sessionStart", hook_command(hook_path, Some("session")))],
        ),
        Target::Mastracode => json_in(
            2,
            "hooks.json",
            HooksRoot::Document,
            &hook_event_commands(hook_path, &super::MASTRACODE_HOOK_EVENTS),
        ),
        Target::AntigravityCli => ancestor(hook_path, 2).is_some_and(|dir| {
            read_json(&dir.join("hooks.json")).is_some_and(|document| {
                document.get(super::ANTIGRAVITY_CLI_HOOK_BLOCK_NAME)
                    == Some(&super::targets::antigravity_cli_hook_block(hook_path))
            })
        }),
        Target::Kimi => ancestor(hook_path, 2)
            .is_some_and(|dir| kimi_hooks_registered(&dir.join("config.toml"), hook_path)),
        // The hook path is `<hermes>/plugins/<plugin>/__init__.py`.
        Target::Hermes => {
            hook_path
                .with_file_name(super::HERMES_PLUGIN_MANIFEST_INSTALL_NAME)
                .is_file()
                && ancestor(hook_path, 3).is_some_and(|dir| {
                    fs::read_to_string(dir.join("config.yaml")).is_ok_and(|config| {
                        super::config_edit::ensure_hermes_plugin_enabled(&config) == config
                    })
                })
        }
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
    target: crate::api::schema::IntegrationTarget,
    path: PathBuf,
    expected_version: u32,
) -> super::IntegrationStatus {
    let (mut state, installed_version) = integration_state_for_path(&path, expected_version);

    // Grok only invokes the hook when the shepr-owned `hooks/shepr.json`
    // registers it, so a current hook script with a missing or broken config
    // is a nonfunctional install: report it as outdated so `shepr integration
    // status` flags it and a reinstall rewrites both files.
    if target == crate::api::schema::IntegrationTarget::Grok
        && state == super::IntegrationStatusKind::Current
        && !grok_hook_config_is_valid(&path)
    {
        state = super::IntegrationStatusKind::Outdated;
    }
    if target == crate::api::schema::IntegrationTarget::Opencode
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

/// Letta is an experimental CLI-only target outside `IntegrationTarget`.
pub(crate) fn experimental_letta_integration_status() -> Option<super::ExperimentalIntegrationStatus>
{
    let path = letta_dir()
        .ok()?
        .join("hooks")
        .join(super::LETTA_HOOK_INSTALL_NAME);
    let (mut state, installed_version) =
        integration_state_for_path(&path, super::LETTA_INTEGRATION_VERSION);
    if state == super::IntegrationStatusKind::Current
        && !ancestor(&path, 2).is_some_and(|dir| {
            json_hook_commands_registered(
                &dir.join("settings.json"),
                HooksRoot::HooksKey,
                &[("SessionStart", hook_command(&path, Some("session")))],
            )
        })
    {
        state = super::IntegrationStatusKind::Outdated;
    }
    Some(super::ExperimentalIntegrationStatus {
        label: "letta",
        path,
        state,
        installed_version,
        expected_version: super::LETTA_INTEGRATION_VERSION,
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
    use crate::api::schema::IntegrationTarget;
    use crate::integration::IntegrationStatusKind;

    fn base(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("shepr-registration-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("test precondition");
        dir
    }

    fn write_current_hook(path: &Path) {
        fs::create_dir_all(path.parent().expect("test precondition")).expect("test precondition");
        fs::write(
            path,
            format!(
                "#!/bin/bash\n# {}1\n",
                super::super::INTEGRATION_VERSION_MARKER
            ),
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
        for (event, action) in super::super::MASTRACODE_HOOK_EVENTS {
            document.insert(
                event.to_string(),
                serde_json::json!([{ "type": "command", "command": hook_command(&hook, Some(action)) }]),
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

    /// Run `install` with `env_var` pointing at `dir`, restoring it after.
    fn with_config_dir<T>(env_var: &str, dir: &Path, install: impl FnOnce() -> T) -> T {
        let original = std::env::var_os(env_var);
        unsafe { std::env::set_var(env_var, dir) };
        let result = install();
        match original {
            Some(value) => unsafe { std::env::set_var(env_var, value) },
            None => unsafe { std::env::remove_var(env_var) },
        }
        result
    }

    #[test]
    fn install_with_unparseable_config_writes_no_hook() {
        let _lock = crate::integration::integration_env_lock();

        let claude = base("claude-malformed");
        fs::write(claude.join("settings.json"), "{ not json").expect("test precondition");
        let result = with_config_dir(
            "CLAUDE_CONFIG_DIR",
            &claude,
            super::super::targets::install_claude,
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
        let result = with_config_dir("CODEX_HOME", &codex, super::super::targets::install_codex);
        assert!(result.is_err());
        assert!(!codex.join(super::super::CODEX_HOOK_INSTALL_NAME).exists());
        let _ = fs::remove_dir_all(codex);

        let copilot = base("copilot-malformed");
        fs::write(copilot.join("settings.json"), "{\"hooks\": []}").expect("test precondition");
        let result = with_config_dir(
            "COPILOT_HOME",
            &copilot,
            super::super::targets::install_copilot,
        );
        assert!(result.is_err());
        assert!(!copilot.join("hooks").exists());
        let _ = fs::remove_dir_all(copilot);
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
