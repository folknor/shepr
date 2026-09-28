use super::command::*;
use super::config_edit::*;
use super::env::*;
use super::registry::*;
use super::targets::*;
use super::types::*;
use super::version::*;
use super::*;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::agent::{KIMI_ASK_USER_QUESTION_MATCHER, KIMI_OTHER_TOOL_MATCHER};
use shepr_core::env::EnvVar;
use shepr_test_support::IsolatedEnv;

use super::test_support::StatPath;

fn install_path(outcome: &InstallOutcome, role: ArtifactRole) -> PathBuf {
    outcome
        .artifacts
        .iter()
        .find(|artifact| artifact.role == role)
        .expect("expected install artifact")
        .path
        .clone()
}

fn uninstall_path(outcome: &UninstallOutcome, role: ArtifactRole) -> PathBuf {
    outcome
        .artifacts
        .iter()
        .find(|artifact| artifact.role == role)
        .expect("expected uninstall artifact")
        .path
        .clone()
}

fn uninstall_was_removed(outcome: &UninstallOutcome, role: ArtifactRole) -> bool {
    outcome
        .artifacts
        .iter()
        .any(|artifact| artifact.role == role && artifact.state == UninstallState::Removed)
}

fn uninstall_was_updated(outcome: &UninstallOutcome, role: ArtifactRole) -> bool {
    outcome
        .artifacts
        .iter()
        .any(|artifact| artifact.role == role && artifact.state == UninstallState::Updated)
}

fn uninstall_paths_with_state(
    outcome: &UninstallOutcome,
    role: ArtifactRole,
    state: UninstallState,
) -> Vec<PathBuf> {
    outcome
        .artifacts
        .iter()
        .filter(|artifact| artifact.role == role && artifact.state == state)
        .map(|artifact| artifact.path.clone())
        .collect()
}

#[test]
fn extract_version_triple_parses_common_outputs() {
    assert_eq!(extract_version_triple("0.14.0"), Some((0, 14, 0)));
    assert_eq!(extract_version_triple("v1.2.3"), Some((1, 2, 3)));
    assert_eq!(
        extract_version_triple("kimi-code 0.14.0 (linux/x64)"),
        Some((0, 14, 0))
    );
    assert_eq!(extract_version_triple("0.14"), Some((0, 14, 0)));
    assert_eq!(extract_version_triple("0.14.1-beta.2"), Some((0, 14, 1)));
    assert_eq!(extract_version_triple("no version here"), None);
    assert_eq!(extract_version_triple(""), None);
}

#[test]
fn extract_version_triple_orders_versions() {
    let old = extract_version_triple("0.12.1").expect("test precondition");
    let min = extract_version_triple(KIMI_MIN_VERSION).expect("test precondition");
    let new = extract_version_triple("0.15.0").expect("test precondition");
    assert!(old < min);
    assert!(min <= min);
    assert!(min < new);
}

#[test]
fn agent_version_requirement_only_set_for_kimi() {
    let requirement = agent_version_requirement(crate::agent::IntegrationTarget::Kimi)
        .expect("kimi must have a version requirement");
    assert_eq!(requirement.binary, "kimi");
    assert_eq!(requirement.min_version, KIMI_MIN_VERSION);
    assert!(agent_version_requirement(crate::agent::IntegrationTarget::Claude).is_none());
    assert!(agent_version_requirement(crate::agent::IntegrationTarget::Codex).is_none());
}

#[test]
fn enforce_agent_version_warns_when_binary_missing() {
    let requirement = AgentVersionRequirement {
        label: "kimi code",
        binary: "shepr-test-binary-that-does-not-exist",
        args: &["--version"],
        min_version: "0.14.0",
    };
    let warning = enforce_agent_version(&requirement, VERSION_PROBE_TIMEOUT)
        .expect("missing binary must not fail the install")
        .expect("missing binary must produce a warning");
    assert!(warning.contains("could not run"));
    assert!(warning.contains("0.14.0"));
}

#[test]
fn enforce_agent_version_rejects_old_version() {
    let requirement = AgentVersionRequirement {
        label: "kimi code",
        binary: "echo",
        args: &["0.12.1"],
        min_version: "0.14.0",
    };
    let err = enforce_agent_version(&requirement, VERSION_PROBE_TIMEOUT)
        .expect_err("old version must fail the install");
    let message = err.to_string();
    assert!(message.contains("0.12.1"));
    assert!(message.contains("0.14.0"));
    assert!(message.contains("upgrade"));
}

#[test]
fn enforce_agent_version_accepts_current_version() {
    let requirement = AgentVersionRequirement {
        label: "kimi code",
        binary: "echo",
        args: &["0.14.0"],
        min_version: "0.14.0",
    };
    let result = enforce_agent_version(&requirement, VERSION_PROBE_TIMEOUT)
        .expect("matching version must not fail the install");
    assert!(result.is_none(), "matching version must not warn");
}

/// Clears the one agent directory override outside the environment registry,
/// so paths resolve against `HOME` unless a test sets one. `IsolatedEnv`
/// clears all registered variables, including every agent config-directory
/// override and the XDG base directories.
fn clear_integration_path_env(env: &IsolatedEnv) {
    env.remove(GROK_CONFIG_DIR_TEST_SEAM);
}

fn kimi_hook_command(hook_path: &Path, action: &str) -> String {
    hook_command(hook_path, Some(action))
}

fn kimi_config_hooks(config: &str) -> Vec<toml::Value> {
    let parsed: toml::Value = toml::from_str(config).expect("test precondition");
    parsed
        .get("hooks")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn assert_kimi_hook(
    config: &str,
    hook_path: &Path,
    event: &str,
    matcher: Option<&str>,
    action: &str,
) {
    let command = kimi_hook_command(hook_path, action);
    let hooks = kimi_config_hooks(config);
    assert!(
        hooks.iter().any(|hook| {
            hook.get("event").and_then(toml::Value::as_str) == Some(event)
                && hook.get("matcher").and_then(toml::Value::as_str) == matcher
                && hook.get("command").and_then(toml::Value::as_str) == Some(command.as_str())
                && hook.get("timeout").and_then(toml::Value::as_integer) == Some(10)
        }),
        "missing kimi hook for {event} ({matcher:?}) -> {action}"
    );
}

/// The directory a test builds its fake homes and agent directories in,
/// inside the test's scratch directory. Also clears the agent overrides.
fn unique_base(env: &IsolatedEnv) -> PathBuf {
    clear_integration_path_env(env);
    env.path().join("base")
}

#[test]
fn install_pi_writes_embedded_asset_to_pi_extensions_dir() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    env.set("HOME", &home);

    let path = install_path(
        &install_pi(&AgentIntegrationPaths::resolve()).expect("test precondition"),
        ArtifactRole::Extension,
    )
    .clone();
    let content = fs::read_to_string(&path).expect("test precondition");

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));
    assert_eq!(content, PI_EXTENSION_ASSET);
}

#[test]
fn install_pi_creates_extensions_dir_when_agent_dir_exists() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let agent_dir = home.join(".pi/agent");
    fs::create_dir_all(&agent_dir).expect("test precondition");
    env.set("HOME", &home);

    let path = install_path(
        &install_pi(&AgentIntegrationPaths::resolve()).expect("test precondition"),
        ArtifactRole::Extension,
    )
    .clone();

    assert_eq!(
        path,
        agent_dir.join("extensions").join(PI_EXTENSION_INSTALL_NAME)
    );
    assert!(path.stat_is_file());
}

#[test]
fn install_pi_uses_pi_coding_agent_dir_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let agent_dir = base.join("custom-pi-agent");
    let ext_dir = agent_dir.join("extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    env.set(EnvVar::PiCodingAgentDir, &agent_dir);

    let path = install_path(
        &install_pi(&AgentIntegrationPaths::resolve()).expect("test precondition"),
        ArtifactRole::Extension,
    )
    .clone();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));

    clear_integration_path_env(&env);
}

#[test]
fn install_pi_expands_tilde_in_pi_coding_agent_dir_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join("custom-pi-agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    env.set("HOME", &home);
    env.set(EnvVar::PiCodingAgentDir, "~/custom-pi-agent");

    let path = install_path(
        &install_pi(&AgentIntegrationPaths::resolve()).expect("test precondition"),
        ArtifactRole::Extension,
    )
    .clone();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));

    clear_integration_path_env(&env);
}

#[test]
fn install_omp_writes_embedded_asset_to_omp_extensions_dir() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join(".omp/agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    env.set("HOME", &home);

    let installed = install_omp(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let content = fs::read_to_string(install_path(&installed, ArtifactRole::Extension))
        .expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Extension),
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert_eq!(content, OMP_EXTENSION_ASSET);
}

#[test]
fn install_omp_uses_omp_config_dir_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join("custom-omp/agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    env.set("HOME", &home);
    env.set(EnvVar::PiConfigDir, "custom-omp");

    let installed = install_omp(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Extension),
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );

    clear_integration_path_env(&env);
}

#[test]
fn install_omp_uses_its_own_config_when_pi_agent_dir_is_set() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let agent_dir = base.join("shared-agent");
    let ext_dir = agent_dir.join("extensions");
    let pi_extension = ext_dir.join(PI_EXTENSION_INSTALL_NAME);
    fs::create_dir_all(&ext_dir).expect("test precondition");
    fs::write(&pi_extension, PI_EXTENSION_ASSET).expect("test precondition");
    let omp_dir = home.join("ignored-omp-config/agent");
    fs::create_dir_all(&omp_dir).expect("test precondition");
    env.set("HOME", &home);
    env.set(EnvVar::PiCodingAgentDir, &agent_dir);
    env.set(EnvVar::PiConfigDir, "ignored-omp-config");

    let installed = install_omp(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Extension),
        omp_dir.join("extensions").join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(pi_extension.stat_is_file());
    assert!(install_path(&installed, ArtifactRole::Extension).stat_is_file());

    clear_integration_path_env(&env);
}

#[test]
fn install_omp_creates_extensions_dir_when_agent_dir_exists() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let agent_dir = home.join(".omp/agent");
    let ext_dir = agent_dir.join("extensions");
    fs::create_dir_all(&agent_dir).expect("test precondition");
    env.set("HOME", &home);

    let installed = install_omp(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Extension),
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(ext_dir.stat_is_dir());
}

#[test]
fn uninstall_omp_removes_embedded_extension_when_present() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join(".omp/agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    fs::write(
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME),
        OMP_EXTENSION_ASSET,
    )
    .expect("test precondition");
    env.set("HOME", &home);

    let result = uninstall_omp(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        uninstall_path(&result, ArtifactRole::Extension),
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(uninstall_was_removed(&result, ArtifactRole::Extension));
    assert!(
        !uninstall_path(&result, ArtifactRole::Extension)
            .try_exists()
            .expect("stat")
    );
}

#[test]
fn install_omp_errors_when_extension_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_omp(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("omp extension directory not found"));
}

#[test]
fn uninstall_pi_removes_embedded_extension_when_present() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    fs::write(ext_dir.join(PI_EXTENSION_INSTALL_NAME), PI_EXTENSION_ASSET)
        .expect("test precondition");
    env.set("HOME", &home);

    let result = uninstall_pi(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        uninstall_path(&result, ArtifactRole::Extension),
        ext_dir.join(PI_EXTENSION_INSTALL_NAME)
    );
    assert!(uninstall_was_removed(&result, ArtifactRole::Extension));
    assert!(
        !uninstall_path(&result, ArtifactRole::Extension)
            .try_exists()
            .expect("stat")
    );
}

#[test]
fn outdated_integrations_treat_missing_version_marker_as_outdated() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    let extension_path = ext_dir.join(PI_EXTENSION_INSTALL_NAME);
    fs::write(&extension_path, "// installed by shepr\n").expect("test precondition");
    env.set("HOME", &home);

    let outdated = outdated_installed_integrations(&AgentIntegrationPaths::resolve());

    assert_eq!(outdated.len(), 1);
    assert_eq!(outdated[0].target, crate::agent::IntegrationTarget::Pi);
    assert_eq!(outdated[0].path, extension_path);
    assert_eq!(outdated[0].installed_version, None);
    assert_eq!(outdated[0].expected_version, PI_INTEGRATION_VERSION);
}

#[test]
fn outdated_integrations_accept_current_version_marker() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    fs::write(ext_dir.join(PI_EXTENSION_INSTALL_NAME), PI_EXTENSION_ASSET)
        .expect("test precondition");
    env.set("HOME", &home);

    assert!(outdated_installed_integrations(&AgentIntegrationPaths::resolve()).is_empty());
}

#[test]
fn install_pi_errors_when_extension_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_pi(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("pi extension directory not found"));
}

#[test]
fn install_claude_writes_hook_and_updates_settings() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).expect("test precondition");
    fs::write(
        claude_dir.join("settings.json"),
        r#"{"permissions":{"allow":["Read"]},"hooks":{}}"#,
    )
    .expect("test precondition");
    env.set("HOME", &home);

    let installed = install_claude(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let hook_content = fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
        .expect("test precondition");
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::Settings))
            .expect("test precondition"),
    )
    .expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        claude_dir.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME)
    );
    assert_eq!(hook_content, CLAUDE_HOOK_ASSET);
    assert!(settings["permissions"]["allow"].is_array());
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["matcher"],
        "^(startup|resume|clear|compact|fork)$"
    );
    assert!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("test precondition")
            .contains(" session")
    );
    assert!(settings["hooks"].get("UserPromptSubmit").is_none());
    assert!(settings["hooks"].get("PreToolUse").is_none());
    assert!(settings["hooks"].get("PermissionRequest").is_none());
    assert!(settings["hooks"].get("PostToolUse").is_none());
    assert!(settings["hooks"].get("PostToolUseFailure").is_none());
    assert!(settings["hooks"].get("SubagentStop").is_none());
    assert!(settings["hooks"].get("Stop").is_none());
    assert!(settings["hooks"].get("SessionEnd").is_none());
}

#[test]
fn install_claude_uses_claude_config_dir_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let claude_dir = base.join("custom-claude");
    fs::create_dir_all(&claude_dir).expect("test precondition");
    env.set(EnvVar::ClaudeConfigDir, &claude_dir);

    let installed = install_claude(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Settings),
        claude_dir.join("settings.json")
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        claude_dir.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME)
    );

    clear_integration_path_env(&env);
}

#[test]
fn install_claude_is_idempotent_for_hook_entries() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).expect("test precondition");
    env.set("HOME", &home);

    install_claude(&AgentIntegrationPaths::resolve()).expect("test precondition");
    install_claude(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(claude_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");
    assert_eq!(
        settings["hooks"]["SessionStart"]
            .as_array()
            .expect("test precondition")
            .len(),
        1
    );
    assert!(settings["hooks"].get("UserPromptSubmit").is_none());
    assert!(settings["hooks"].get("PreToolUse").is_none());
    assert!(settings["hooks"].get("PermissionRequest").is_none());
    assert!(settings["hooks"].get("PostToolUse").is_none());
    assert!(settings["hooks"].get("PostToolUseFailure").is_none());
    assert!(settings["hooks"].get("SubagentStop").is_none());
    assert!(settings["hooks"].get("Stop").is_none());
    assert!(settings["hooks"].get("SessionEnd").is_none());
}

#[test]
fn uninstall_claude_removes_shepr_hooks_and_preserves_others() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    let hooks_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).expect("test precondition");
    let hook_path = hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);
    fs::write(&hook_path, CLAUDE_HOOK_ASSET).expect("test precondition");
    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' session", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep", "timeout": 10}
                ]
            }]
        }
    });
    fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string(&settings).expect("test precondition"),
    )
    .expect("test precondition");
    env.set("HOME", &home);

    let result = uninstall_claude(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(claude_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");

    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(&result, ArtifactRole::Settings));
    assert!(
        !uninstall_path(&result, ArtifactRole::Hook)
            .try_exists()
            .expect("stat")
    );
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"]
            .as_array()
            .expect("test precondition")
            .len(),
        1
    );
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "echo keep"
    );
}

#[test]
fn install_claude_errors_when_claude_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_claude(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("claude directory not found"));
}

#[test]
fn install_codex_writes_hook_and_updates_hooks_and_config() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).expect("test precondition");
    fs::write(codex_dir.join("config.toml"), "model = \"gpt-5.4\"\n").expect("test precondition");
    env.set("HOME", &home);

    let installed = install_codex(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let hook_content = fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
        .expect("test precondition");
    let hooks: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::Hooks))
            .expect("test precondition"),
    )
    .expect("test precondition");
    let config = fs::read_to_string(install_path(&installed, ArtifactRole::Config))
        .expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        codex_dir.join(CODEX_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Hooks),
        codex_dir.join("hooks.json")
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Config),
        codex_dir.join("config.toml")
    );
    assert_eq!(hook_content, CODEX_HOOK_ASSET);
    assert!(
        hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("test precondition")
            .contains(" session")
    );
    assert!(hooks["hooks"].get("UserPromptSubmit").is_none());
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert!(hooks["hooks"].get("Stop").is_none());
    assert!(config.contains("model = \"gpt-5.4\""));
    assert!(config.contains("[features]"));
    assert!(config.contains("hooks = true"));
    assert!(!config.contains("codex_hooks"));
}

#[test]
fn install_codex_uses_codex_home_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let codex_dir = base.join("custom-codex");
    fs::create_dir_all(&codex_dir).expect("test precondition");
    fs::write(codex_dir.join("config.toml"), "model = \"gpt-5.4\"\n").expect("test precondition");
    env.set(EnvVar::CodexHome, &codex_dir);

    let installed = install_codex(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        codex_dir.join(CODEX_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Hooks),
        codex_dir.join("hooks.json")
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Config),
        codex_dir.join("config.toml")
    );

    clear_integration_path_env(&env);
}

#[test]
fn install_codex_is_idempotent_for_hook_entries_and_feature_flag() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).expect("test precondition");
    fs::write(
        codex_dir.join("config.toml"),
        "[features]\ncodex_hooks = false\nother = true\n",
    )
    .expect("test precondition");
    env.set("HOME", &home);

    install_codex(&AgentIntegrationPaths::resolve()).expect("test precondition");
    install_codex(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let hooks: Value = serde_json::from_str(
        &fs::read_to_string(codex_dir.join("hooks.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let config = fs::read_to_string(codex_dir.join("config.toml")).expect("test precondition");

    assert_eq!(
        hooks["hooks"]["SessionStart"]
            .as_array()
            .expect("test precondition")
            .len(),
        1
    );
    assert!(hooks["hooks"].get("UserPromptSubmit").is_none());
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert!(hooks["hooks"].get("Stop").is_none());
    assert_eq!(config.matches("hooks = true").count(), 1);
    assert!(!config.contains("codex_hooks"));
    assert!(config.contains("other = true"));
}

#[test]
fn install_codex_only_migrates_top_level_feature_flags() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).expect("test precondition");
    fs::write(
            codex_dir.join("config.toml"),
            "profile = \"work\"\n\n[profiles.work.features]\nhooks = false\ncodex_hooks = false\n\n[features]\ncodex_hooks = true\nother = true\n",
        )
        .expect("test precondition");
    env.set("HOME", &home);

    install_codex(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let config = fs::read_to_string(codex_dir.join("config.toml")).expect("test precondition");

    assert!(config.contains("[profiles.work.features]\nhooks = false\ncodex_hooks = false"));
    let parsed: toml::Table = toml::from_str(&config).expect("valid TOML");
    let features = parsed
        .get("features")
        .and_then(toml::Value::as_table)
        .expect("features table");
    assert_eq!(features.get("hooks"), Some(&toml::Value::Boolean(true)));
    assert_eq!(features.get("other"), Some(&toml::Value::Boolean(true)));
    assert!(!features.contains_key("codex_hooks"), "{config}");
    assert_eq!(config.matches("[features]").count(), 1, "{config}");
}

#[test]
fn uninstall_codex_removes_shepr_hooks_and_leaves_config_alone() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).expect("test precondition");
    let hook_path = codex_dir.join(CODEX_HOOK_INSTALL_NAME);
    fs::write(&hook_path, CODEX_HOOK_ASSET).expect("test precondition");
    let hooks = serde_json::json!({
        "hooks": {
            "SessionStart": [{"hooks": [
                {"type": "command", "command": format!("bash '{}' session", hook_path.display()), "timeout": 10},
                {"type": "command", "command": "echo keep", "timeout": 10}
            ]}]
        }
    });
    fs::write(
        codex_dir.join("hooks.json"),
        serde_json::to_string(&hooks).expect("test precondition"),
    )
    .expect("test precondition");
    fs::write(
        codex_dir.join("config.toml"),
        "[features]\nhooks = true\nother = true\n",
    )
    .expect("test precondition");
    env.set("HOME", &home);

    let result = uninstall_codex(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let hooks: Value = serde_json::from_str(
        &fs::read_to_string(codex_dir.join("hooks.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let config = fs::read_to_string(codex_dir.join("config.toml")).expect("test precondition");

    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(&result, ArtifactRole::Hooks));
    assert!(
        !uninstall_path(&result, ArtifactRole::Hook)
            .try_exists()
            .expect("stat")
    );
    assert_eq!(
        hooks["hooks"]["SessionStart"][0]["hooks"]
            .as_array()
            .expect("test precondition")
            .len(),
        1
    );
    assert_eq!(
        hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "echo keep"
    );
    assert!(config.contains("hooks = true"));
    assert!(config.contains("other = true"));
}

#[test]
fn install_codex_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_codex(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("codex config directory not found"));
}

#[test]
fn install_kimi_writes_hook_and_updates_config() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).expect("test precondition");
    fs::write(
            kimi_dir.join("config.toml"),
            "default_model = \"moonshot\"\n\n[[hooks]]\nevent = \"Notification\"\nmatcher = \"task.completed\"\ncommand = \"echo keep\"\ntimeout = 3\n",
        )
        .expect("test precondition");
    env.set("HOME", &home);

    let installed = install_kimi(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let hook_content = fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
        .expect("test precondition");
    let config = fs::read_to_string(install_path(&installed, ArtifactRole::Config))
        .expect("test precondition");
    let hooks = kimi_config_hooks(&config);

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        kimi_dir.join("hooks").join(KIMI_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Config),
        kimi_dir.join("config.toml")
    );
    assert_eq!(hook_content, KIMI_HOOK_ASSET);
    assert_eq!(hooks.len(), KIMI_HOOK_EVENTS.len() + 1);
    assert!(config.contains("default_model = \"moonshot\""));
    assert!(config.contains("command = \"echo keep\""));
    assert!(config.contains(KIMI_CONFIG_BLOCK_BEGIN));
    assert!(config.contains(KIMI_CONFIG_BLOCK_END));
    for hook in KIMI_HOOK_EVENTS {
        let action = hook
            .action
            .map(crate::agent::IntegrationHookAction::as_str)
            .expect("Kimi hook action should be present");
        assert_kimi_hook(
            &config,
            &install_path(&installed, ArtifactRole::Hook),
            hook.event,
            hook.matcher,
            action,
        );
    }
}

#[test]
fn kimi_question_hooks_report_blocked_until_the_question_finishes() {
    let has_event = |event, matcher, action| {
        KIMI_HOOK_EVENTS.iter().any(|hook| {
            hook.event == event && hook.matcher == matcher && hook.action == Some(action)
        })
    };
    assert!(has_event(
        "PreToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        crate::agent::IntegrationHookAction::Blocked,
    ));
    assert!(has_event(
        "PostToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        crate::agent::IntegrationHookAction::Working,
    ));
    assert!(has_event(
        "PostToolUseFailure",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        crate::agent::IntegrationHookAction::Working,
    ));
    assert!(has_event(
        "PreToolUse",
        Some(KIMI_OTHER_TOOL_MATCHER),
        crate::agent::IntegrationHookAction::Working,
    ));
}

#[test]
fn install_kimi_uses_kimi_code_home_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let kimi_dir = base.join("custom-kimi");
    fs::create_dir_all(&kimi_dir).expect("test precondition");
    env.set(EnvVar::KimiCodeHome, &kimi_dir);

    let installed = install_kimi(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        kimi_dir.join("hooks").join(KIMI_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Config),
        kimi_dir.join("config.toml")
    );

    clear_integration_path_env(&env);
}

#[test]
fn install_kimi_is_idempotent_for_config_block() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).expect("test precondition");
    env.set("HOME", &home);

    install_kimi(&AgentIntegrationPaths::resolve()).expect("test precondition");
    install_kimi(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let config = fs::read_to_string(kimi_dir.join("config.toml")).expect("test precondition");
    let hooks = kimi_config_hooks(&config);

    assert_eq!(config.matches(KIMI_CONFIG_BLOCK_BEGIN).count(), 1);
    assert_eq!(config.matches(KIMI_CONFIG_BLOCK_END).count(), 1);
    assert_eq!(hooks.len(), KIMI_HOOK_EVENTS.len());
}

#[test]
fn uninstall_kimi_removes_hook_and_config_block_preserves_other_hooks() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).expect("test precondition");
    env.set("HOME", &home);

    let installed = install_kimi(&AgentIntegrationPaths::resolve()).expect("test precondition");
    fs::write(
            install_path(&installed, ArtifactRole::Config),
            format!(
                "default_model = \"moonshot\"\n\n[[hooks]]\nevent = \"Notification\"\ncommand = \"echo keep\"\n\n{}",
                fs::read_to_string(install_path(&installed, ArtifactRole::Config)).expect("test precondition")
            ),
        )
        .expect("test precondition");

    let result = uninstall_kimi(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let config = fs::read_to_string(kimi_dir.join("config.toml")).expect("test precondition");
    let hooks = kimi_config_hooks(&config);

    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(&result, ArtifactRole::Config));
    assert!(
        !uninstall_path(&result, ArtifactRole::Hook)
            .try_exists()
            .expect("stat")
    );
    assert!(config.contains("default_model = \"moonshot\""));
    assert!(config.contains("command = \"echo keep\""));
    assert!(!config.contains(KIMI_CONFIG_BLOCK_BEGIN));
    assert!(!config.contains(KIMI_CONFIG_BLOCK_END));
    assert_eq!(hooks.len(), 1);
    assert_eq!(
        hooks[0].get("event").and_then(toml::Value::as_str),
        Some("Notification")
    );
}

#[test]
fn install_kimi_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_kimi(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("kimi code config directory not found"));
}

#[test]
fn install_copilot_writes_hook_and_updates_settings() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let copilot_dir = home.join(".copilot");
    fs::create_dir_all(&copilot_dir).expect("test precondition");
    fs::write(
        copilot_dir.join("settings.json"),
        r#"{"theme":"dark","hooks":{"PreToolUse":[{"type":"command","command":"echo keep","timeoutSec":10}]}}"#,
    )
    .expect("test precondition");
    env.set("HOME", &home);

    let installed = install_copilot(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let hook_content = fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
        .expect("test precondition");
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::Settings))
            .expect("test precondition"),
    )
    .expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        copilot_dir.join("hooks").join(COPILOT_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Settings),
        copilot_dir.join("settings.json")
    );
    assert_eq!(hook_content, COPILOT_HOOK_ASSET);
    assert_eq!(settings["theme"], "dark");
    assert_eq!(
        settings["hooks"]["PreToolUse"]
            .as_array()
            .expect("test precondition")
            .len(),
        1
    );
    assert_eq!(settings["hooks"]["PreToolUse"][0]["command"], "echo keep");
    assert!(
        settings["hooks"]["SessionStart"][0][direct_command_field()]
            .as_str()
            .expect("test precondition")
            .contains(COPILOT_HOOK_INSTALL_NAME)
    );
    // The hook action travels in the command, never as a tool matcher.
    for (event, entries) in settings["hooks"].as_object().expect("hooks object") {
        for entry in entries.as_array().expect("hook list") {
            assert!(entry.get("matcher").is_none(), "{event}: {entry}");
        }
    }
}

#[test]
fn install_copilot_uses_copilot_home_env_and_is_idempotent() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let copilot_dir = base.join("custom-copilot");
    fs::create_dir_all(&copilot_dir).expect("test precondition");
    env.set(EnvVar::CopilotHome, &copilot_dir);

    let installed = install_copilot(&AgentIntegrationPaths::resolve()).expect("test precondition");
    install_copilot(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(copilot_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        copilot_dir.join("hooks").join(COPILOT_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        settings["hooks"]["SessionStart"]
            .as_array()
            .expect("test precondition")
            .len(),
        1
    );

    clear_integration_path_env(&env);
}

#[test]
fn uninstall_copilot_removes_shepr_hooks_and_preserves_others() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let copilot_dir = home.join(".copilot");
    let hooks_dir = copilot_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).expect("test precondition");
    let hook_path = hooks_dir.join(COPILOT_HOOK_INSTALL_NAME);
    fs::write(&hook_path, COPILOT_HOOK_ASSET).expect("test precondition");
    let command = format!(
        "bash {}",
        shell_single_quote(&hook_path.display().to_string())
    );
    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {"type": "command", direct_command_field(): command, "timeoutSec": 10},
                {"type": "command", "command": "echo keep", "timeoutSec": 10}
            ]
        }
    });
    fs::write(
        copilot_dir.join("settings.json"),
        serde_json::to_string(&settings).expect("test precondition"),
    )
    .expect("test precondition");
    env.set("HOME", &home);

    let result = uninstall_copilot(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(copilot_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");

    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(&result, ArtifactRole::Settings));
    assert!(
        !uninstall_path(&result, ArtifactRole::Hook)
            .try_exists()
            .expect("stat")
    );
    assert_eq!(
        settings["hooks"]["SessionStart"]
            .as_array()
            .expect("test precondition")
            .len(),
        1
    );
    assert_eq!(settings["hooks"]["SessionStart"][0]["command"], "echo keep");
}

#[test]
fn install_copilot_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_copilot(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("copilot config directory not found"));
}

#[test]
fn install_devin_writes_hook_and_updates_settings() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let xdg_config = base.join("xdg");
    let devin_dir = xdg_config.join("devin");
    fs::create_dir_all(&devin_dir).expect("test precondition");
    fs::write(
        devin_dir.join("config.json"),
        r#"{"theme_mode":"dark","hooks":{}}"#,
    )
    .expect("test precondition");
    env.set("XDG_CONFIG_HOME", &xdg_config);
    env.set("HOME", base.join("home"));

    let installed = install_devin(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let hook_content = fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
        .expect("test precondition");
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::Settings))
            .expect("test precondition"),
    )
    .expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        devin_dir.join(DEVIN_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Settings),
        devin_dir.join("config.json")
    );
    assert_eq!(hook_content, DEVIN_HOOK_ASSET);
    assert_eq!(settings["theme_mode"], "dark");
    for hook in DEVIN_HOOK_EVENTS {
        let action = hook
            .action
            .map(crate::agent::IntegrationHookAction::as_str)
            .expect("Devin hook action should be present");
        let command = settings["hooks"][hook.event][0]["hooks"][0]["command"]
            .as_str()
            .expect("test precondition");
        assert!(
            command.contains(DEVIN_HOOK_INSTALL_NAME) && command.ends_with(action),
            "expected devin {} hook command to end with {action}, got {command}",
            hook.event
        );
    }

    clear_integration_path_env(&env);
}

#[test]
fn install_devin_is_idempotent_for_hook_entries() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let xdg_config = base.join("xdg");
    let devin_dir = xdg_config.join("devin");
    fs::create_dir_all(&devin_dir).expect("test precondition");
    env.set("XDG_CONFIG_HOME", &xdg_config);
    env.set("HOME", base.join("home"));

    install_devin(&AgentIntegrationPaths::resolve()).expect("test precondition");
    install_devin(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(devin_dir.join("config.json")).expect("test precondition"),
    )
    .expect("test precondition");
    for hook in DEVIN_HOOK_EVENTS {
        assert_eq!(
            settings["hooks"][hook.event]
                .as_array()
                .expect("test precondition")
                .len(),
            1,
            "expected hooks.{} to be idempotent",
            hook.event
        );
    }

    clear_integration_path_env(&env);
}

#[test]
fn uninstall_devin_removes_shepr_hooks_and_preserves_others() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let xdg_config = base.join("xdg");
    let devin_dir = xdg_config.join("devin");
    fs::create_dir_all(&devin_dir).expect("test precondition");
    env.set("XDG_CONFIG_HOME", &xdg_config);
    env.set("HOME", base.join("home"));

    install_devin(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let hook_path = devin_dir.join(DEVIN_HOOK_INSTALL_NAME);
    let mut settings: Value = serde_json::from_str(
        &fs::read_to_string(devin_dir.join("config.json")).expect("test precondition"),
    )
    .expect("test precondition");
    settings["hooks"]["UserPromptSubmit"]
        .as_array_mut()
        .expect("test precondition")
        .push(json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": "echo keep",
                "timeout": 10
            }]
        }));
    fs::write(
        devin_dir.join("config.json"),
        serde_json::to_string_pretty(&settings).expect("test precondition"),
    )
    .expect("test precondition");

    let result = uninstall_devin(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(devin_dir.join("config.json")).expect("test precondition"),
    )
    .expect("test precondition");

    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(&result, ArtifactRole::Settings));
    assert!(!hook_path.try_exists().expect("stat"));
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"]
            .as_array()
            .expect("test precondition")
            .len(),
        1
    );
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "echo keep"
    );
    assert!(settings["hooks"].get("SessionStart").is_none());
    assert!(settings["hooks"].get("PreToolUse").is_none());
    assert!(settings["hooks"].get("PermissionRequest").is_none());
    assert!(settings["hooks"].get("Stop").is_none());
    assert!(settings["hooks"].get("SessionEnd").is_none());

    clear_integration_path_env(&env);
}

#[test]
fn install_devin_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let xdg_config = base.join("xdg");
    fs::create_dir_all(&xdg_config).expect("test precondition");
    env.set("XDG_CONFIG_HOME", &xdg_config);
    env.set("HOME", base.join("home"));

    let err = install_devin(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();
    assert!(err.contains("devin config directory not found"));

    clear_integration_path_env(&env);
}

#[test]
fn install_droid_writes_hook_to_settings() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let droid_dir = home.join(".factory");
    fs::create_dir_all(&droid_dir).expect("test precondition");
    fs::write(
        droid_dir.join("settings.json"),
        r#"{"theme":"factory-dark"}"#,
    )
    .expect("test precondition");
    env.set("HOME", &home);

    let installed = install_droid(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let hook_content = fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
        .expect("test precondition");
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::Hooks))
            .expect("test precondition"),
    )
    .expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        droid_dir.join("hooks").join(DROID_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Hooks),
        droid_dir.join("settings.json")
    );
    assert_eq!(hook_content, DROID_HOOK_ASSET);
    assert_eq!(settings["theme"], "factory-dark");
    assert!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("test precondition")
            .contains(DROID_HOOK_INSTALL_NAME)
    );
    assert!(
        settings["hooks"]["SessionStart"][0]
            .get("matcher")
            .is_none()
    );
    for hook in DROID_HOOK_EVENTS {
        let action = hook
            .action
            .map(crate::agent::IntegrationHookAction::as_str)
            .expect("Droid hook action should be present");
        let command = settings["hooks"][hook.event][0]["hooks"][0]["command"]
            .as_str()
            .expect("test precondition");
        assert!(
            command.contains(DROID_HOOK_INSTALL_NAME) && command.ends_with(action),
            "expected droid {} hook command to end with {action}, got {command}",
            hook.event
        );
    }
}

#[test]
fn install_droid_is_idempotent_for_hook_entries() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let droid_dir = home.join(".factory");
    fs::create_dir_all(&droid_dir).expect("test precondition");
    env.set("HOME", &home);

    install_droid(&AgentIntegrationPaths::resolve()).expect("test precondition");
    install_droid(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(droid_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");
    for hook in DROID_HOOK_EVENTS {
        assert_eq!(
            settings["hooks"][hook.event]
                .as_array()
                .expect("test precondition")
                .len(),
            1,
            "expected hooks.{} to be idempotent",
            hook.event
        );
    }
}

#[test]
fn uninstall_droid_removes_shepr_hooks_and_preserves_others() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let droid_dir = home.join(".factory");
    let hooks_dir = droid_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).expect("test precondition");
    let hook_path = hooks_dir.join(DROID_HOOK_INSTALL_NAME);
    fs::write(&hook_path, DROID_HOOK_ASSET).expect("test precondition");
    let command = hook_command(&hook_path, Some("session"));
    fs::write(
            droid_dir.join("settings.json"),
            format!(
                r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":{},"timeout":10}}]}}],"PostToolUse":[{{"matcher":"Edit","hooks":[{{"type":"command","command":"echo post","timeout":10}}]}}]}}}}"#,
                serde_json::to_string(&command).expect("test precondition"),
            ),
        )
        .expect("test precondition");
    env.set("HOME", &home);

    let result = uninstall_droid(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(droid_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");

    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(&result, ArtifactRole::Settings));
    assert!(
        !uninstall_path(&result, ArtifactRole::Hook)
            .try_exists()
            .expect("stat")
    );
    assert!(settings["hooks"].get("SessionStart").is_none());
    assert_eq!(settings["hooks"]["PostToolUse"][0]["matcher"], "Edit");
}

#[test]
fn install_droid_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_droid(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("droid config directory not found"));
}

#[test]
fn install_opencode_writes_server_and_tui_plugins() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).expect("test precondition");
    env.set("HOME", &home);

    let installed = install_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Plugin),
        opencode_dir
            .join("plugins")
            .join(OPENCODE_PLUGIN_INSTALL_NAME)
    );
    assert_eq!(
        fs::read_to_string(install_path(&installed, ArtifactRole::Plugin))
            .expect("test precondition"),
        OPENCODE_PLUGIN_ASSET
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::TuiPlugin),
        opencode_dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME)
    );
    assert_eq!(
        fs::read_to_string(install_path(&installed, ArtifactRole::TuiPlugin))
            .expect("test precondition"),
        OPENCODE_TUI_PLUGIN_ASSET
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::TuiConfig),
        opencode_dir.join("tui.jsonc")
    );
    let tui_config: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::TuiConfig))
            .expect("test precondition"),
    )
    .expect("test precondition");
    assert_eq!(tui_config["plugin"], json!([OPENCODE_TUI_PLUGIN_SPEC]));
    let cli_config_path = opencode_dir.join("cli.json");
    assert_eq!(cli_config_path, opencode_dir.join("cli.json"));
    let cli_config: Value =
        serde_json::from_str(&fs::read_to_string(&cli_config_path).expect("test precondition"))
            .expect("test precondition");
    assert_eq!(cli_config["plugins"], json!([OPENCODE_V2_TUI_PLUGIN_SPEC]));
}

#[test]
fn opencode_reuses_json_registration_in_symlinked_config_directory() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let dotfiles = base.join("dotfiles");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(home.join(".config")).expect("test precondition");
    fs::create_dir_all(&dotfiles).expect("test precondition");
    std::os::unix::fs::symlink(&dotfiles, &dir).expect("test precondition");
    env.set("HOME", &home);
    let json_path = dir.join("tui.json");
    let original = "{\n  // User preferences\n  \"theme\":\"system\",\n  \"plugin\":[\"other\",[\"./shepr-tui-session.js\",{\"enabled\":true}]]\n}\n";
    fs::write(&json_path, original).expect("test precondition");

    for _ in 0..2 {
        let installed =
            install_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");
        assert_eq!(install_path(&installed, ArtifactRole::TuiConfig), json_path);

        assert!(!dir.join("tui.jsonc").try_exists().expect("stat"));
        assert_eq!(
            fs::read_to_string(&json_path).expect("test precondition"),
            original
        );
        assert_eq!(
            integration_status_at(
                crate::agent::IntegrationTarget::Opencode,
                install_path(&installed, ArtifactRole::Plugin),
                OPENCODE_INTEGRATION_VERSION,
            )
            .expect("stat plugin")
            .state,
            IntegrationStatusKind::Current
        );
    }

    // OpenCode reads both files, so the plugin can be registered in each (a
    // hand-added entry beside shepr's); uninstall removes it from both.
    let jsonc_path = dir.join("tui.jsonc");
    fs::write(
        &jsonc_path,
        r#"{"plugin":["./shepr-tui-session.js","another"]}"#,
    )
    .expect("test precondition");
    assert_eq!(
        install_path(
            &install_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition"),
            ArtifactRole::TuiConfig
        ),
        jsonc_path
    );
    let result = uninstall_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert_eq!(
        uninstall_paths_with_state(&result, ArtifactRole::TuiConfig, UninstallState::Updated),
        vec![jsonc_path.clone(), json_path.clone()]
    );
    let json = fs::read_to_string(&json_path).expect("test precondition");
    assert!(json.contains("// User preferences"));
    assert!(!json.contains("shepr-tui-session.js"));
    assert!(json.contains("\"other\""));
    assert!(json.contains("\"system\""));
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(jsonc_path).expect("test precondition"))
            .expect("test precondition"),
        json!({"plugin":["another"]})
    );
    assert!(
        uninstall_paths_with_state(
            &uninstall_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition"),
            ArtifactRole::TuiConfig,
            UninstallState::Updated
        )
        .is_empty()
    );
    assert_eq!(fs::read_link(&dir).expect("test precondition"), dotfiles);
    assert!(
        !uninstall_path(&result, ArtifactRole::Plugin)
            .try_exists()
            .expect("stat")
    );
    assert!(
        !uninstall_path(&result, ArtifactRole::TuiPlugin)
            .try_exists()
            .expect("stat")
    );
    fs::remove_dir_all(base).expect("test precondition");
}

#[test]
fn opencode_install_defers_v2_registration_while_migration_pending() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).expect("test precondition");
    fs::write(opencode_dir.join("tui.json"), "{}").expect("test precondition");
    env.set("HOME", &home);

    install_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert!(!opencode_dir.join("cli.json").try_exists().expect("stat"));
    assert!(
        opencode_dir
            .join(OPENCODE_V2_TUI_PLUGIN_DIR)
            .join("tui.js")
            .stat_is_file()
    );
}

#[test]
fn opencode_v2_install_status_and_uninstall_preserve_cli_preferences() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(&dir).expect("test precondition");
    env.set("HOME", &home);
    let cli = dir.join("cli.json");
    fs::write(
        &cli,
        r#"{"theme":{"name":"catppuccin"},"plugins":["other"]}"#,
    )
    .expect("test precondition");
    let installed = install_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let status = || {
        integration_status_at(
            crate::agent::IntegrationTarget::Opencode,
            install_path(&installed, ArtifactRole::Plugin).clone(),
            OPENCODE_INTEGRATION_VERSION,
        )
        .expect("stat plugin")
        .state
    };
    assert_eq!(status(), IntegrationStatusKind::Current);
    let entry = dir.join(OPENCODE_V2_TUI_PLUGIN_DIR).join("tui.js");
    assert_eq!(
        fs::read_to_string(&entry).expect("test precondition"),
        OPENCODE_V2_TUI_PLUGIN_ASSET
    );
    fs::remove_file(&entry).expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    install_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");
    super::opencode_config::remove_cli_plugin(&dir, OPENCODE_V2_TUI_PLUGIN_SPEC)
        .expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    install_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");
    uninstall_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(!entry.try_exists().expect("stat"));
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(cli).expect("test precondition"))
            .expect("test precondition"),
        json!({"theme":{"name":"catppuccin"},"plugins":["other"]})
    );
}

#[test]
fn opencode_hard_link_rejection_precedes_install_and_uninstall_asset_changes() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).expect("test precondition");
    env.set("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").expect("test precondition");
    let config = dir.join("cli.json");
    let original = r#"{"plugins":["./shepr-opencode"],"theme":"system"}"#;
    fs::write(&config, original).expect("test precondition");
    let alias = base.join("linked-config");
    fs::hard_link(&config, &alias).expect("test precondition");
    let target = crate::agent::IntegrationTarget::Opencode;
    for error in [
        install_target(&AgentIntegrationPaths::resolve(), target).expect_err("test precondition"),
        uninstall_target(&AgentIntegrationPaths::resolve(), target).expect_err("test precondition"),
    ] {
        assert!(error.to_string().contains("multiple hard links"));
        assert!(error.to_string().contains("cli.json"));
    }
    assert_eq!(
        fs::read_to_string(&plugin).expect("test precondition"),
        "previous integration"
    );
    assert_eq!(
        fs::read_to_string(&alias).expect("test precondition"),
        original
    );
    assert_eq!(
        shepr_platform::config_file_link_count(&config).expect("test precondition"),
        2
    );
    assert!(!dir.join("tui.jsonc").try_exists().expect("stat"));
    assert!(
        !dir.join(OPENCODE_V2_TUI_PLUGIN_DIR)
            .try_exists()
            .expect("stat")
    );
    fs::remove_dir_all(base).expect("test precondition");
}

#[test]
fn opencode_json_config_validation_precedes_asset_changes() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).expect("test precondition");
    env.set("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").expect("test precondition");
    let config = dir.join("tui.json");
    fs::write(&config, r#"{"plugin":{}}"#).expect("test precondition");
    assert!(
        install_opencode(&AgentIntegrationPaths::resolve())
            .expect_err("test precondition")
            .to_string()
            .contains("plugin list")
    );
    assert_eq!(
        fs::read_to_string(&plugin).expect("test precondition"),
        "previous integration"
    );
    let original = r#"{"plugin":["./shepr-tui-session.js"]}"#;
    fs::write(&config, original).expect("test precondition");
    let alias = base.join("linked-config");
    fs::hard_link(&config, &alias).expect("test precondition");
    for error in [
        install_opencode(&AgentIntegrationPaths::resolve()).expect_err("test precondition"),
        uninstall_opencode(&AgentIntegrationPaths::resolve()).expect_err("test precondition"),
    ] {
        assert!(error.to_string().contains("multiple hard links"));
        assert!(error.to_string().contains("tui.json"));
    }
    assert_eq!(
        fs::read_to_string(&plugin).expect("test precondition"),
        "previous integration"
    );
    assert_eq!(
        fs::read_to_string(&alias).expect("test precondition"),
        original
    );
    assert!(!dir.join("tui.jsonc").try_exists().expect("stat"));
    assert!(
        !dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME)
            .try_exists()
            .expect("stat")
    );
    fs::remove_dir_all(base).expect("test precondition");
}

#[test]
fn opencode_invalid_cli_config_does_not_overwrite_existing_plugins() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).expect("test precondition");
    env.set("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").expect("test precondition");
    fs::write(dir.join("cli.json"), r#"{"plugins":{}}"#).expect("test precondition");
    assert!(install_opencode(&AgentIntegrationPaths::resolve()).is_err());
    assert_eq!(
        fs::read_to_string(plugin).expect("test precondition"),
        "previous integration"
    );
    assert!(!dir.join("tui.jsonc").try_exists().expect("stat"));
}

#[test]
fn opencode_status_requires_the_tui_plugin_and_config_entry() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).expect("test precondition");
    env.set("HOME", &home);
    let installed = install_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let status = || {
        integration_status_at(
            crate::agent::IntegrationTarget::Opencode,
            install_path(&installed, ArtifactRole::Plugin).clone(),
            OPENCODE_INTEGRATION_VERSION,
        )
        .expect("stat plugin")
        .state
    };

    assert_eq!(status(), IntegrationStatusKind::Current);
    fs::remove_file(install_path(&installed, ArtifactRole::TuiPlugin)).expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    fs::write(
        install_path(&installed, ArtifactRole::TuiPlugin),
        OPENCODE_TUI_PLUGIN_ASSET,
    )
    .expect("test precondition");
    super::opencode_config::remove_tui_plugin(&opencode_dir, OPENCODE_TUI_PLUGIN_SPEC)
        .expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Outdated);
}

#[test]
fn uninstall_opencode_removes_plugins_and_managed_tui_config_entry() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).expect("test precondition");
    env.set("HOME", &home);
    let installed = install_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let result = uninstall_opencode(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert!(uninstall_was_removed(&result, ArtifactRole::Plugin));
    assert!(uninstall_was_removed(&result, ArtifactRole::TuiPlugin));
    assert_eq!(
        uninstall_paths_with_state(&result, ArtifactRole::TuiConfig, UninstallState::Updated),
        vec![install_path(&installed, ArtifactRole::TuiConfig).clone()]
    );
    assert!(
        !uninstall_path(&result, ArtifactRole::Plugin)
            .try_exists()
            .expect("stat")
    );
    assert!(
        !uninstall_path(&result, ArtifactRole::TuiPlugin)
            .try_exists()
            .expect("stat")
    );
    assert!(
        install_path(&installed, ArtifactRole::TuiConfig)
            .try_exists()
            .expect("stat")
    );
    let tui_config: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::TuiConfig))
            .expect("test precondition"),
    )
    .expect("test precondition");
    assert_eq!(tui_config, json!({}));
    assert_eq!(
        install_path(&installed, ArtifactRole::Plugin),
        uninstall_path(&result, ArtifactRole::Plugin)
    );
}

#[test]
fn install_opencode_invalid_tui_config_does_not_write_plugins() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).expect("test precondition");
    fs::write(opencode_dir.join("tui.jsonc"), r#"{"plugin":{}}"#).expect("test precondition");
    env.set("HOME", &home);

    let err = install_opencode(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("plugin list"));
    assert!(
        !opencode_dir
            .join("plugins")
            .join(OPENCODE_PLUGIN_INSTALL_NAME)
            .try_exists()
            .expect("stat")
    );
    assert!(
        !opencode_dir
            .join(OPENCODE_TUI_PLUGIN_INSTALL_NAME)
            .try_exists()
            .expect("stat")
    );
}

#[test]
fn uninstall_opencode_removes_plugins_when_tui_config_is_invalid() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    let plugins_dir = opencode_dir.join("plugins");
    fs::create_dir_all(&plugins_dir).expect("test precondition");
    let plugin_path = plugins_dir.join(OPENCODE_PLUGIN_INSTALL_NAME);
    let tui_plugin_path = opencode_dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME);
    fs::write(&plugin_path, OPENCODE_PLUGIN_ASSET).expect("test precondition");
    fs::write(&tui_plugin_path, OPENCODE_TUI_PLUGIN_ASSET).expect("test precondition");
    fs::write(opencode_dir.join("tui.jsonc"), "{\"plugin\":").expect("test precondition");
    let json_path = opencode_dir.join("tui.json");
    fs::write(
        &json_path,
        r#"{"plugin":["./shepr-tui-session.js","other"]}"#,
    )
    .expect("test precondition");
    env.set("HOME", &home);

    let err = uninstall_opencode(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("failed to parse OpenCode TUI config"));
    assert!(!plugin_path.try_exists().expect("stat"));
    assert!(!tui_plugin_path.try_exists().expect("stat"));
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(json_path).expect("test precondition"))
            .expect("test precondition"),
        json!({"plugin":["other"]})
    );
}

#[test]
fn install_opencode_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_opencode(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("opencode config directory not found"));
}

#[test]
fn install_kilo_writes_plugin_to_plugin_dir() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let kilo_dir = home.join(".config/kilo");
    fs::create_dir_all(&kilo_dir).expect("test precondition");
    env.set("HOME", &home);

    let installed = install_kilo(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let plugin_content = fs::read_to_string(install_path(&installed, ArtifactRole::Plugin))
        .expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Plugin),
        kilo_dir.join("plugin").join(KILO_PLUGIN_INSTALL_NAME)
    );
    assert_eq!(plugin_content, KILO_PLUGIN_ASSET);
}

#[test]
fn uninstall_kilo_removes_plugin_when_present() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let kilo_plugin_dir = home.join(".config/kilo/plugin");
    fs::create_dir_all(&kilo_plugin_dir).expect("test precondition");
    fs::write(
        kilo_plugin_dir.join(KILO_PLUGIN_INSTALL_NAME),
        KILO_PLUGIN_ASSET,
    )
    .expect("test precondition");
    env.set("HOME", &home);

    let result = uninstall_kilo(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert!(uninstall_was_removed(&result, ArtifactRole::Plugin));
    assert!(
        !uninstall_path(&result, ArtifactRole::Plugin)
            .try_exists()
            .expect("stat")
    );
}

#[test]
fn install_kilo_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_kilo(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("kilo config directory not found"));
}

#[test]
fn process_owned_integration_assets_do_not_report_release() {
    for (name, asset) in [
        ("pi", PI_EXTENSION_ASSET),
        ("omp", OMP_EXTENSION_ASSET),
        ("mastracode", MASTRACODE_HOOK_ASSET),
        ("kimi", KIMI_HOOK_ASSET),
        ("kilo", KILO_PLUGIN_ASSET),
    ] {
        assert!(
            !asset.contains("pane.release_agent"),
            "{name} process exit should own lifecycle release"
        );
    }
}

#[test]
fn opencode_family_plugins_keep_child_sessions_off_the_pane() {
    // Both are full-lifecycle authorities: a subagent session must neither
    // replace the pane's resumable session nor mark the pane idle, and a
    // `{ type: ... }` status object must be understood.
    for (name, asset) in [
        ("opencode", OPENCODE_PLUGIN_ASSET),
        ("kilo", KILO_PLUGIN_ASSET),
    ] {
        for needle in [
            "childSessions.set(info.id, info.parentID)",
            "if (sessionID && childSessions.has(sessionID))",
            "CHILD_EVENT_STATES.get(type)",
            "status?.type",
        ] {
            assert!(
                asset.contains(needle),
                "{name} plugin is missing `{needle}`"
            );
        }
    }
}

#[test]
fn pi_extension_refreshes_session_ref_before_agent_start_state() {
    let agent_start = PI_EXTENSION_ASSET
        .find("pi.on(\"agent_start\", (_event, ctx)")
        .expect("pi extension should receive agent_start context");
    let handler = &PI_EXTENSION_ASSET[agent_start..];
    let update_session = handler
        .find("updateSessionRef(ctx);")
        .expect("pi extension should refresh the active session on agent_start");
    let report_session = handler
        .find("void reportSession();")
        .expect("pi extension should report the refreshed session before state");
    let publish_state = handler
        .find("publishState();")
        .expect("pi extension should publish working state after refreshing session");

    assert!(update_session < report_session);
    assert!(report_session < publish_state);
}

#[test]
fn omp_extension_refreshes_session_ref_before_agent_start_state() {
    let agent_start = OMP_EXTENSION_ASSET
        .find("pi.on(\"agent_start\", (_event, ctx)")
        .expect("omp extension should receive agent_start context");
    let handler = &OMP_EXTENSION_ASSET[agent_start..];
    let update_session = handler
        .find("updateSessionRef(ctx);")
        .expect("omp extension should refresh the active session on agent_start");
    let report_session = handler
        .find("void reportSession();")
        .expect("omp extension should report the refreshed session before state");
    let publish_state = handler
        .find("publishState();")
        .expect("omp extension should publish working state after refreshing session");

    assert!(update_session < report_session);
    assert!(report_session < publish_state);
}

fn omp_handler(event: &str) -> &'static str {
    let start = OMP_EXTENSION_ASSET
        .find(&format!("pi.on(\"{event}\""))
        .unwrap_or_else(|| panic!("omp extension registers {event} handler"));
    let rest = &OMP_EXTENSION_ASSET[start..];
    let end = rest[1..]
        .find("\n\n  pi.")
        .map_or(rest.len(), |offset| offset + 1);
    &rest[..end]
}

#[test]
fn omp_root_activation_requires_ui_context() {
    let activator = OMP_EXTENSION_ASSET
        .find("function activateRootSession(ctx: any, sessionStartSource = \"startup\"): boolean")
        .expect("omp extension should centralize root session activation");
    let helper = &OMP_EXTENSION_ASSET[activator..];
    let non_ui_guard = helper
        .find("ctx?.hasUI !== true")
        .expect("omp extension checks UI context before activating");
    let root_session = helper
        .find("rootSession = true;")
        .expect("omp extension activates root session after UI guard");
    let session_report = helper
        .find("void reportSession(sessionStartSource);")
        .expect("omp extension reports root session");

    assert!(non_ui_guard < root_session);
    assert!(root_session < session_report);
}

#[test]
fn omp_session_start_and_switch_use_root_activation() {
    let session_start = OMP_EXTENSION_ASSET
        .find("pi.on(\"session_start\", (_event, ctx)")
        .expect("omp extension registers session_start handler");
    let session_start_handler = &OMP_EXTENSION_ASSET[session_start..];
    session_start_handler
        .find("if (!activateRootSession(ctx))")
        .expect("omp session_start handler should activate root session");

    let session_switch = OMP_EXTENSION_ASSET
        .find("pi.on(\"session_switch\", (event, ctx)")
        .expect("omp extension registers session_switch handler");
    let session_switch_handler = &OMP_EXTENSION_ASSET[session_switch..];
    session_switch_handler
        .find("if (!activateRootSession(ctx, event?.reason || \"resume\"))")
        .expect("omp session_switch handler should activate root session with switch reason");
}

#[test]
fn omp_session_reports_include_start_source() {
    let report_session = OMP_EXTENSION_ASSET
        .find("function reportSession(sessionStartSource = \"startup\"): Promise<void>")
        .expect("omp extension should label session reports with a lifecycle source");
    let helper = &OMP_EXTENSION_ASSET[report_session..];
    let session_source = helper
        .find("session_start_source: sessionStartSource")
        .expect("omp session reports should include the lifecycle source");
    let session_ref = helper
        .find("...sessionRef")
        .expect("omp session reports should include the native session ref");

    assert!(session_source < session_ref);
}

#[test]
fn omp_socket_requests_are_serialized() {
    let queue = OMP_EXTENSION_ASSET
        .find("let requestQueue = Promise.resolve();")
        .expect("omp extension should keep socket reports ordered");
    let send_request = OMP_EXTENSION_ASSET[queue..]
        .find("function sendRequest(request: unknown): Promise<void>")
        .expect("omp extension should wrap socket sends in an ordered queue");
    let queued_send = OMP_EXTENSION_ASSET[queue + send_request..]
        .find("requestQueue = requestQueue.then(")
        .expect("omp extension should serialize socket requests through the queue");
    let raw_send = OMP_EXTENSION_ASSET[queue + send_request..]
        .find("sendRequestNow(request)")
        .expect("omp extension should enqueue the raw socket send");

    assert!(queued_send < raw_send);
}

#[test]
fn omp_runtime_events_can_activate_root_session_after_resume() {
    for event in [
        "agent_start",
        "tool_approval_requested",
        "tool_approval_resolved",
        "tool_execution_start",
        "tool_execution_end",
    ] {
        let handler = omp_handler(event);
        handler
            .find("!rootSession && !activateRootSession(ctx)")
            .unwrap_or_else(|| panic!("omp {event} handler should recover missing root session"));
    }
}

#[test]
fn omp_ask_and_approval_events_report_blocked_state() {
    let approval_handler = omp_handler("tool_approval_requested");
    approval_handler
        .find("activateBlocked(label);")
        .expect("approval requests should block the pane");

    let approval_resolved = omp_handler("tool_approval_resolved");
    approval_resolved
        .find("deactivateBlocked();")
        .expect("approval resolution should unblock the pane");

    let ask_handler = omp_handler("tool_execution_start");
    ask_handler
        .find("event?.toolName !== \"ask\"")
        .expect("tool execution handler should only treat Ask as blocked");
    ask_handler
        .find("activateBlocked(askBlockedMessage(event.args));")
        .expect("Ask start should block the pane");

    let ask_end_handler = omp_handler("tool_execution_end");
    ask_end_handler
        .find("event?.toolName !== \"ask\"")
        .expect("tool execution end should only treat Ask as blocked");
    ask_end_handler
        .find("deactivateBlocked();")
        .expect("Ask end should unblock the pane");
}

#[test]
fn install_qodercli_writes_hook_and_updates_settings() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let qoder_dir = base.join(".qoder");
    fs::create_dir_all(&qoder_dir).expect("test precondition");
    fs::write(
        qoder_dir.join("settings.json"),
        r#"{"permissions":{"allow":["Read"]},"hooks":{}}"#,
    )
    .expect("test precondition");
    env.set(EnvVar::QoderConfigDir, &qoder_dir);

    let installed = install_qodercli(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        qoder_dir.join("hooks").join(QODERCLI_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Settings),
        qoder_dir.join("settings.json")
    );
    assert!(install_path(&installed, ArtifactRole::Hook).stat_is_file());

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::Settings))
            .expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = settings
        .get("hooks")
        .and_then(Value::as_object)
        .expect("hooks should be present");
    for hook in QODERCLI_HOOK_EVENTS {
        let action = hook
            .action
            .map(crate::agent::IntegrationHookAction::as_str)
            .expect("QoderCLI hook action should be present");
        assert!(
            hooks.contains_key(hook.event),
            "expected hooks.{} to be registered",
            hook.event
        );
        let command = hooks[hook.event][0]["hooks"][0]["command"]
            .as_str()
            .expect("test precondition");
        assert!(
            command.contains(QODERCLI_HOOK_INSTALL_NAME) && command.ends_with(action),
            "expected qodercli {} hook command to end with {action}, got {command}",
            hook.event
        );
    }
    // Pre-existing settings keys must be preserved.
    assert!(settings.get("permissions").is_some());

    env.remove(EnvVar::QoderConfigDir);
}

#[test]
fn install_qodercli_is_idempotent_for_hook_entries() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let qoder_dir = base.join(".qoder");
    fs::create_dir_all(&qoder_dir).expect("test precondition");
    env.set(EnvVar::QoderConfigDir, &qoder_dir);

    install_qodercli(&AgentIntegrationPaths::resolve()).expect("test precondition");
    install_qodercli(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(qoder_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = settings
        .get("hooks")
        .and_then(Value::as_object)
        .expect("test precondition");
    for hook in QODERCLI_HOOK_EVENTS {
        let entries = hooks
            .get(hook.event)
            .and_then(Value::as_array)
            .expect("test precondition");
        assert_eq!(
            entries.len(),
            1,
            "expected hooks.{} to contain exactly one entry, got {entries:?}",
            hook.event
        );
    }

    env.remove(EnvVar::QoderConfigDir);
}

#[test]
fn uninstall_qodercli_removes_shepr_hooks_and_preserves_others() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let qoder_dir = base.join(".qoder");
    fs::create_dir_all(&qoder_dir).expect("test precondition");
    env.set(EnvVar::QoderConfigDir, &qoder_dir);

    install_qodercli(&AgentIntegrationPaths::resolve()).expect("test precondition");
    // Inject a foreign hook entry the user might have configured by hand.
    let mut settings: Value = serde_json::from_str(
        &fs::read_to_string(qoder_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");
    settings["hooks"]["SessionStart"]
        .as_array_mut()
        .expect("test precondition")
        .push(json!({
            "matcher": "*",
            "hooks": [{"type": "command", "command": "echo user-defined"}],
        }));
    fs::write(
        qoder_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).expect("test precondition"),
    )
    .expect("test precondition");

    let result = uninstall_qodercli(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(&result, ArtifactRole::Settings));

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(qoder_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = settings
        .get("hooks")
        .and_then(Value::as_object)
        .expect("test precondition");
    let remaining = hooks
        .get("SessionStart")
        .and_then(Value::as_array)
        .expect("test precondition");
    assert_eq!(remaining.len(), 1);
    let cmd = remaining[0]["hooks"][0]["command"]
        .as_str()
        .expect("test precondition");
    assert_eq!(cmd, "echo user-defined");

    env.remove(EnvVar::QoderConfigDir);
}

#[test]
fn install_qodercli_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let missing = base.join(".qoder");
    env.set(EnvVar::QoderConfigDir, &missing);

    let err = install_qodercli(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();
    assert!(
        err.contains("qodercli config directory not found"),
        "unexpected error: {err}"
    );

    env.remove(EnvVar::QoderConfigDir);
}

#[test]
fn install_qwen_writes_session_hook_and_preserves_settings() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let qwen_dir = base.join(".qwen");
    fs::create_dir_all(&qwen_dir).expect("test precondition");
    fs::write(
        qwen_dir.join("settings.json"),
        r#"{"permissions":{"allow":["Read"]},"hooks":{}}"#,
    )
    .expect("test precondition");
    env.set(EnvVar::QwenHome, &qwen_dir);

    let installed = install_qwen(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        qwen_dir.join("hooks").join(QWEN_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Settings),
        qwen_dir.join("settings.json")
    );
    assert!(install_path(&installed, ArtifactRole::Hook).stat_is_file());

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::Settings))
            .expect("test precondition"),
    )
    .expect("test precondition");
    let entries = settings["hooks"]["SessionStart"]
        .as_array()
        .expect("test precondition");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["matcher"], "*");
    assert_eq!(entries[0]["hooks"][0]["timeout"], 10_000);
    let command = entries[0]["hooks"][0]["command"]
        .as_str()
        .expect("test precondition");
    assert!(command.contains(QWEN_HOOK_INSTALL_NAME));
    assert!(command.ends_with("session"));
    assert!(settings.get("permissions").is_some());
    let hook_asset = fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
        .expect("test precondition");
    assert!(hook_asset.contains("SHEPR_INTEGRATION_ID=qwen"));
    assert!(hook_asset.contains("SHEPR_INTEGRATION_VERSION=1"));
    assert!(hook_asset.contains("shepr:qwen"));

    install_qwen(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::Settings))
            .expect("test precondition"),
    )
    .expect("test precondition");
    assert_eq!(
        settings["hooks"]["SessionStart"]
            .as_array()
            .expect("test precondition")
            .len(),
        1
    );

    env.remove(EnvVar::QwenHome);
}

#[test]
fn uninstall_qwen_removes_only_shepr_hook() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let qwen_dir = base.join(".qwen");
    fs::create_dir_all(&qwen_dir).expect("test precondition");
    env.set(EnvVar::QwenHome, &qwen_dir);

    install_qwen(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let settings_path = qwen_dir.join("settings.json");
    let mut settings: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).expect("test precondition"))
            .expect("test precondition");
    settings["hooks"]["SessionStart"]
        .as_array_mut()
        .expect("test precondition")
        .push(json!({
            "matcher": "resume",
            "hooks": [{"type": "command", "command": "echo user-defined"}],
        }));
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&settings).expect("test precondition"),
    )
    .expect("test precondition");

    let result = uninstall_qwen(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(&result, ArtifactRole::Settings));

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).expect("test precondition"))
            .expect("test precondition");
    let remaining = settings["hooks"]["SessionStart"]
        .as_array()
        .expect("test precondition");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["hooks"][0]["command"], "echo user-defined");

    env.remove(EnvVar::QwenHome);
}

#[test]
fn install_qwen_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let missing = base.join(".qwen");
    env.set(EnvVar::QwenHome, &missing);

    let err = install_qwen(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();
    assert!(err.contains("qwen code config directory not found"));

    env.remove(EnvVar::QwenHome);
}

#[test]
fn install_and_uninstall_letta_preserve_unrelated_settings_and_hooks() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let letta_dir = home.join(".letta");
    fs::create_dir_all(&letta_dir).expect("test precondition");
    let settings_path = letta_dir.join("settings.json");
    fs::write(
        &settings_path,
        r#"{"theme":"dark","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo user"}]}]}}"#,
    )
    .expect("test precondition");
    env.set("HOME", &home);

    let installed = install_letta(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        letta_dir.join("hooks").join(LETTA_HOOK_INSTALL_NAME)
    );
    let first_install = fs::read_to_string(&settings_path).expect("test precondition");
    let settings: Value = serde_json::from_str(&first_install).expect("test precondition");
    let entries = settings["hooks"]["SessionStart"]
        .as_array()
        .expect("test precondition");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["hooks"][0]["command"], "echo user");
    assert!(entries[1].get("matcher").is_none());
    assert_eq!(entries[1]["hooks"][0]["timeout"], LETTA_HOOK_TIMEOUT_MS);
    assert_eq!(entries[1]["hooks"][0]["quiet"], true);
    assert!(
        entries[1]["hooks"][0]["command"]
            .as_str()
            .expect("test precondition")
            .ends_with("session")
    );
    assert_eq!(settings["theme"], "dark");

    install_letta(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert_eq!(
        fs::read_to_string(&settings_path).expect("test precondition"),
        first_install
    );

    let result = uninstall_letta(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(
        &result,
        ArtifactRole::SingleEntrySettings
    ));
    assert!(
        !install_path(&installed, ArtifactRole::Hook)
            .try_exists()
            .expect("stat")
    );
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).expect("test precondition"))
            .expect("test precondition");
    assert_eq!(settings["theme"], "dark");
    let remaining = settings["hooks"]["SessionStart"]
        .as_array()
        .expect("test precondition");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["hooks"][0]["command"], "echo user");
}

#[test]
fn letta_session_hook_is_silent_and_encodes_default_conversation() {
    use shepr_test_support::fixture::{self, Step};
    use std::io::Write;
    use std::process::Stdio;

    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(home.join(".letta")).expect("test precondition");
    env.set("HOME", &home);
    let installed = install_letta(&AgentIntegrationPaths::resolve()).expect("test precondition");

    // The shepr the hook reports through: a fixture stand-in recording the
    // arguments it was given, one per line.
    let capture = base.join("args.txt");
    let fake_shepr = fixture::stand_in(
        &base,
        "shepr",
        &[Step::To(capture.clone()), Step::PrintArgs],
    );

    // host-program-ok: the shipped hook asset is the subject, run as its agent runs it
    let mut child = shepr_test_support::command_in_scratch("sh", "letta-session-hook")
        .arg(install_path(&installed, ArtifactRole::Hook))
        .arg("session")
        .env("SHEPR_ENV", "1")
        .env("SHEPR_PANE_ID", "w1:p2")
        .env("SHEPR_SOCKET_PATH", "/tmp/shepr.sock")
        .env("SHEPR_BIN_PATH", &fake_shepr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("test precondition");
    child
        .stdin
        .take()
        .expect("test precondition")
        .write_all(
            br#"{"event_type":"SessionStart","conversation_id":"default","agent_id":"agent-123","is_new_session":false}"#,
        )
        .expect("test precondition");
    let output = child.wait_with_output().expect("test precondition");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let args = fs::read_to_string(capture)
        .expect("test precondition")
        .lines()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(args.contains("report-agent-session w1:p2"));
    assert!(args.contains("--source shepr:letta --agent letta"));
    assert!(args.contains("--agent-session-id default:agent-123"));
    assert!(args.contains("--session-start-source resume"));
}

/// Runs the bundled Kimi hook with `payload` on stdin and returns the request
/// line it sent to a stand-in server socket, or `None` when it sent nothing.
/// The hook needs python3; callers skip when [`python3_available`] is false.
#[expect(
    clippy::unwrap_in_result,
    reason = "the expect calls are test preconditions (fixture setup), not the None path this function's return type communicates to callers"
)]
fn run_kimi_hook(base: &Path, action: &str, payload: &[u8]) -> Option<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::process::Stdio;

    fs::create_dir_all(base).expect("test precondition");
    let hook = base.join(KIMI_HOOK_INSTALL_NAME);
    fs::write(&hook, KIMI_HOOK_ASSET).expect("test precondition");
    let socket_path = base.join("s.sock");
    let listener = UnixListener::bind(&socket_path).expect("test precondition");
    listener.set_nonblocking(true).expect("test precondition");

    // host-program-ok: the shipped hook asset is the subject, run as its agent runs it
    let mut child = shepr_test_support::command_in_scratch("sh", "kimi-hook")
        .arg(&hook)
        .arg(action)
        .env("SHEPR_ENV", "1")
        .env("SHEPR_PANE_ID", "w1:p2")
        .env("SHEPR_SOCKET_PATH", &socket_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("test precondition");
    child
        .stdin
        .take()
        .expect("test precondition")
        .write_all(payload)
        .expect("test precondition");
    let status = child.wait().expect("test precondition");
    assert!(status.success(), "the hook must never fail its caller");

    // The hook has exited; a connection it made is still queued on the
    // listener with its request buffered.
    let (mut stream, _) = listener.accept().ok()?;
    stream.set_nonblocking(false).expect("test precondition");
    let mut request = String::new();
    stream
        .read_to_string(&mut request)
        .expect("test precondition");
    Some(request)
}

fn python3_available() -> bool {
    // host-program-ok: the shipped python hook assets are the subject
    shepr_test_support::command_in_scratch("python3", "python3-probe")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn kimi_hook_reports_state_when_the_payload_is_not_a_json_object() {
    let env = IsolatedEnv::new();
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let base = unique_base(&env);

    let payloads: [&[u8]; 5] = [
        b"[1, 2]",
        b"\"text\"",
        b"null",
        b"not json",
        br#"{"session_id":"abc"}"#,
    ];
    for (index, payload) in payloads.into_iter().enumerate() {
        let request = run_kimi_hook(&base.join(index.to_string()), "working", payload)
            .unwrap_or_else(|| {
                panic!(
                    "payload {:?} sent no report",
                    String::from_utf8_lossy(payload)
                )
            });
        let request: Value = serde_json::from_str(request.trim()).expect("test precondition");
        assert_eq!(request["method"], "pane.report_agent");
        assert_eq!(request["params"]["state"], "working");
        assert_eq!(request["params"]["pane_id"], "w1:p2");
    }
}

/// Runs a session-only python hook asset with `payload` on stdin. Returns the
/// hook's exit success, its stderr, and the request it sent to a stand-in
/// server socket (if any).
fn run_session_hook(base: &Path, asset: &str, payload: &[u8]) -> (bool, Vec<u8>, Option<String>) {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::process::Stdio;

    fs::create_dir_all(base).expect("test precondition");
    let hook = base.join("hook.sh");
    fs::write(&hook, asset).expect("test precondition");
    let socket_path = base.join("s.sock");
    let listener = UnixListener::bind(&socket_path).expect("test precondition");
    listener.set_nonblocking(true).expect("test precondition");

    // host-program-ok: the shipped hook asset is the subject, run as its agent runs it
    let mut child = shepr_test_support::command_in_scratch("sh", "session-hook")
        .arg(&hook)
        .arg("session")
        .env("SHEPR_ENV", "1")
        .env("SHEPR_PANE_ID", "w1:p2")
        .env("SHEPR_SOCKET_PATH", &socket_path)
        .env("TMPDIR", base)
        // Inherited agent variables change what these hooks report.
        .env_remove("CURSOR_VERSION")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("GROK_SESSION_ID")
        // Without a session id in the payload the Devin hook asks `devin list`;
        // pin that lookup to an empty list so the test never runs a real binary.
        .env("SHEPR_DEVIN_LIST_JSON", "[]")
        .env_remove("DEVIN_PROJECT_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("test precondition");
    child
        .stdin
        .take()
        .expect("test precondition")
        .write_all(payload)
        .expect("test precondition");
    let output = child.wait_with_output().expect("test precondition");

    let request = listener.accept().ok().map(|(mut stream, _)| {
        stream.set_nonblocking(false).expect("test precondition");
        let mut request = String::new();
        stream
            .read_to_string(&mut request)
            .expect("test precondition");
        request
    });
    (output.status.success(), output.stderr, request)
}

#[test]
fn session_hooks_ignore_non_object_payloads_quietly() {
    let env = IsolatedEnv::new();
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let base = unique_base(&env);
    let hooks: [(&str, &str, &[u8]); 8] = [
        (
            "claude",
            CLAUDE_HOOK_ASSET,
            br#"{"hook_event_name":"SessionStart","session_id":"abc"}"#,
        ),
        (
            "codex",
            CODEX_HOOK_ASSET,
            br#"{"hook_event_name":"SessionStart","session_id":"abc","transcript_path":"/t"}"#,
        ),
        (
            "grok",
            GROK_HOOK_ASSET,
            br#"{"hook_event_name":"session_start","session_id":"abc"}"#,
        ),
        ("droid", DROID_HOOK_ASSET, br#"{"session_id":"abc"}"#),
        (
            "cursor",
            CURSOR_HOOK_ASSET,
            br#"{"hook_event_name":"sessionStart","session_id":"abc"}"#,
        ),
        (
            "copilot",
            COPILOT_HOOK_ASSET,
            br#"{"hook_event_name":"sessionStart","sessionId":"abc"}"#,
        ),
        (
            "devin",
            DEVIN_HOOK_ASSET,
            br#"{"hook_event_name":"SessionStart","session_id":"abc"}"#,
        ),
        // Mastracode also reports state; its `session` action is the one that
        // must send nothing without a session id.
        (
            "mastracode",
            MASTRACODE_HOOK_ASSET,
            br#"{"session_id":"abc"}"#,
        ),
    ];
    let non_objects: [&[u8]; 4] = [b"[1, 2]", b"\"text\"", b"null", b"7"];

    for (name, asset, valid) in hooks {
        for (index, payload) in non_objects.into_iter().enumerate() {
            let (success, stderr, request) =
                run_session_hook(&base.join(format!("{name}-{index}")), asset, payload);
            let shown = String::from_utf8_lossy(payload);
            assert!(success, "{name} hook failed on {shown}");
            assert!(
                stderr.is_empty(),
                "{name} hook wrote to stderr on {shown}: {}",
                String::from_utf8_lossy(&stderr)
            );
            assert!(request.is_none(), "{name} hook reported on {shown}");
        }

        let (success, _, request) =
            run_session_hook(&base.join(format!("{name}-valid")), asset, valid);
        assert!(success, "{name} hook failed on a valid payload");
        let request = request.unwrap_or_else(|| panic!("{name} hook sent no session report"));
        let request: Value = serde_json::from_str(request.trim()).expect("test precondition");
        assert_eq!(request["method"], "pane.report_agent_session");
        assert_eq!(request["params"]["agent_session_id"], "abc");
    }
}

/// A python exception the payload tests cannot provoke must still not fail the
/// agent's hook or print a traceback: every shell hook runs its python with
/// stderr discarded and its exit status ignored.
#[test]
fn shell_hooks_never_let_python_fail_the_hook() {
    for (name, asset) in [
        ("claude", CLAUDE_HOOK_ASSET),
        ("codex", CODEX_HOOK_ASSET),
        ("kimi", KIMI_HOOK_ASSET),
        ("copilot", COPILOT_HOOK_ASSET),
        ("devin", DEVIN_HOOK_ASSET),
        ("droid", DROID_HOOK_ASSET),
        ("qodercli", QODERCLI_HOOK_ASSET),
        ("qwen", QWEN_HOOK_ASSET),
        ("letta", LETTA_HOOK_ASSET),
        ("cursor", CURSOR_HOOK_ASSET),
        ("antigravity_cli", ANTIGRAVITY_CLI_HOOK_ASSET),
        ("mastracode", MASTRACODE_HOOK_ASSET),
        ("grok", GROK_HOOK_ASSET),
    ] {
        let heredoc = asset.contains("python3 - 2>/dev/null <<'PY' || true");
        let inline = asset.contains("python3 -c '")
            && asset
                .lines()
                .any(|line| line.starts_with('\'') && line.ends_with("2>/dev/null || true"));
        assert!(
            heredoc || inline,
            "{name} hook must run python as `2>/dev/null ... || true`"
        );
    }
}

#[test]
fn install_letta_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_letta(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();
    assert!(err.contains("letta code config directory not found"));
}

#[test]
fn install_letta_does_not_publish_hook_when_settings_are_invalid() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let letta_dir = home.join(".letta");
    fs::create_dir_all(&letta_dir).expect("test precondition");
    fs::write(letta_dir.join("settings.json"), "not json").expect("test precondition");
    env.set("HOME", &home);

    assert!(install_letta(&AgentIntegrationPaths::resolve()).is_err());
    assert!(
        !letta_dir
            .join("hooks")
            .join(LETTA_HOOK_INSTALL_NAME)
            .try_exists()
            .expect("stat")
    );
}

#[test]
fn letta_install_and_uninstall_keep_symlinked_settings_and_reject_hard_links() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let letta_dir = home.join(".letta");
    let dotfiles = base.join("dotfiles");
    fs::create_dir_all(&letta_dir).expect("test precondition");
    fs::create_dir_all(&dotfiles).expect("test precondition");
    let real_settings = dotfiles.join("letta-settings.json");
    fs::write(&real_settings, r#"{"theme":"dark"}"#).expect("test precondition");
    let settings_path = letta_dir.join("settings.json");
    std::os::unix::fs::symlink(&real_settings, &settings_path).expect("test precondition");
    env.set("HOME", &home);

    install_letta(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(
        fs::symlink_metadata(&settings_path)
            .expect("test precondition")
            .file_type()
            .is_symlink(),
        "install must write through the symlink, not replace it"
    );
    let installed: Value =
        serde_json::from_str(&fs::read_to_string(&real_settings).expect("test precondition"))
            .expect("test precondition");
    assert_eq!(installed["theme"], "dark");
    assert!(installed["hooks"]["SessionStart"].is_array());

    let result = uninstall_letta(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(uninstall_was_updated(
        &result,
        ArtifactRole::SingleEntrySettings
    ));
    assert!(
        fs::symlink_metadata(&settings_path)
            .expect("test precondition")
            .file_type()
            .is_symlink(),
        "uninstall must write through the symlink, not replace it"
    );
    let uninstalled: Value =
        serde_json::from_str(&fs::read_to_string(&real_settings).expect("test precondition"))
            .expect("test precondition");
    assert_eq!(uninstalled["theme"], "dark");

    fs::remove_file(&settings_path).expect("test precondition");
    fs::hard_link(&real_settings, &settings_path).expect("test precondition");
    assert!(install_letta(&AgentIntegrationPaths::resolve()).is_err());
    assert!(
        !letta_dir
            .join("hooks")
            .join(LETTA_HOOK_INSTALL_NAME)
            .try_exists()
            .expect("stat"),
        "a rejected settings target must not leave a hook behind"
    );
    assert!(uninstall_letta(&AgentIntegrationPaths::resolve()).is_err());
}

#[test]
fn install_cursor_writes_hook_and_updates_hooks_json() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let cursor_dir = base.join(".cursor");
    fs::create_dir_all(&cursor_dir).expect("test precondition");
    fs::write(
        cursor_dir.join("hooks.json"),
        r#"{"version":1,"hooks":{"stop":[{"command":"echo keep-me"}]}}"#,
    )
    .expect("test precondition");
    env.set(EnvVar::CursorConfigDir, &cursor_dir);

    let installed = install_cursor(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        cursor_dir.join(CURSOR_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::UpdatedHooks),
        cursor_dir.join("hooks.json")
    );
    assert_eq!(
        fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
            .expect("test precondition"),
        CURSOR_HOOK_ASSET
    );

    let hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(cursor_dir.join("hooks.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = hooks_file
        .get("hooks")
        .and_then(Value::as_object)
        .expect("test precondition");
    let session_start = hooks
        .get("sessionStart")
        .and_then(Value::as_array)
        .expect("test precondition");
    assert_eq!(session_start.len(), 1);
    assert_eq!(
        session_start[0].get("command").and_then(Value::as_str),
        Some(
            hook_command(
                &install_path(&installed, ArtifactRole::Hook),
                Some("session")
            )
            .as_str()
        )
    );
    assert!(hooks.get("beforeSubmitPrompt").is_none());
    assert!(hooks.get("beforeShellExecution").is_none());
    let stop = hooks
        .get("stop")
        .and_then(Value::as_array)
        .expect("test precondition");
    assert_eq!(stop.len(), 1);
    assert_eq!(
        stop[0].get("command").and_then(Value::as_str),
        Some("echo keep-me")
    );

    env.remove(EnvVar::CursorConfigDir);
}

#[test]
fn install_cursor_is_idempotent_for_hook_entries() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let cursor_dir = base.join(".cursor");
    fs::create_dir_all(&cursor_dir).expect("test precondition");
    env.set(EnvVar::CursorConfigDir, &cursor_dir);

    install_cursor(&AgentIntegrationPaths::resolve()).expect("test precondition");
    install_cursor(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(cursor_dir.join("hooks.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = hooks_file
        .get("hooks")
        .and_then(Value::as_object)
        .expect("test precondition");
    let session_start = hooks
        .get("sessionStart")
        .and_then(Value::as_array)
        .expect("test precondition");
    assert_eq!(session_start.len(), 1);

    env.remove(EnvVar::CursorConfigDir);
}

#[test]
fn uninstall_cursor_removes_shepr_hooks_and_preserves_others() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let cursor_dir = base.join(".cursor");
    fs::create_dir_all(&cursor_dir).expect("test precondition");
    env.set(EnvVar::CursorConfigDir, &cursor_dir);

    install_cursor(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let mut hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(cursor_dir.join("hooks.json")).expect("test precondition"),
    )
    .expect("test precondition");
    hooks_file["hooks"]["beforeSubmitPrompt"] = json!([{ "command": "echo user-defined" }]);
    fs::write(
        cursor_dir.join("hooks.json"),
        serde_json::to_string_pretty(&hooks_file).expect("test precondition"),
    )
    .expect("test precondition");

    let result = uninstall_cursor(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(&result, ArtifactRole::Hooks));
    assert!(!cursor_dir.join(CURSOR_HOOK_INSTALL_NAME).stat_is_file());

    let hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(cursor_dir.join("hooks.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = hooks_file
        .get("hooks")
        .and_then(Value::as_object)
        .expect("test precondition");
    assert!(!hooks.contains_key("sessionStart"));
    assert!(hooks.contains_key("beforeSubmitPrompt"));

    env.remove(EnvVar::CursorConfigDir);
}

#[test]
fn install_cursor_uses_cursor_config_dir_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let cursor_dir = base.join("custom-cursor");
    fs::create_dir_all(&cursor_dir).expect("test precondition");
    env.set(EnvVar::CursorConfigDir, &cursor_dir);

    let installed = install_cursor(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        cursor_dir.join(CURSOR_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::UpdatedHooks),
        cursor_dir.join("hooks.json")
    );

    clear_integration_path_env(&env);
}

#[test]
fn cursor_integration_status_is_current_after_install() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let cursor_dir = base.join(".cursor");
    fs::create_dir_all(&cursor_dir).expect("test precondition");
    env.set(EnvVar::CursorConfigDir, &cursor_dir);
    // A hook script alone is not enough: the agent's hooks.json must also
    // register it, so install through the real path instead of hand-writing
    // just the script.
    install_cursor(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let statuses = installed_integration_statuses(&AgentIntegrationPaths::resolve());
    let cursor = statuses
        .iter()
        .find(|status| status.target == crate::agent::IntegrationTarget::Cursor)
        .expect("cursor integration status");
    assert_eq!(cursor.state, IntegrationStatusKind::Current);
    assert_eq!(cursor.installed_version, Some(CURSOR_INTEGRATION_VERSION));

    clear_integration_path_env(&env);
}

#[test]
fn install_cursor_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let missing = base.join(".cursor");
    env.set(EnvVar::CursorConfigDir, &missing);

    let err = install_cursor(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();
    assert!(
        err.contains("cursor config directory not found"),
        "unexpected error: {err}"
    );

    env.remove(EnvVar::CursorConfigDir);
}

#[test]
fn install_mastracode_writes_hook_and_updates_hooks_json() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let mastracode_dir = base.join(".mastracode");
    fs::create_dir_all(&mastracode_dir).expect("test precondition");
    fs::write(
        mastracode_dir.join("hooks.json"),
        r#"{"PostToolUse":[{"type":"command","command":"echo keep-me"}]}"#,
    )
    .expect("test precondition");
    env.set("HOME", &base);

    let installed =
        install_mastracode(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        mastracode_dir
            .join("hooks")
            .join(MASTRACODE_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Hooks),
        mastracode_dir.join("hooks.json")
    );
    assert_eq!(
        fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
            .expect("test precondition"),
        MASTRACODE_HOOK_ASSET
    );

    let hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(mastracode_dir.join("hooks.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = hooks_file.as_object().expect("test precondition");
    for hook in MASTRACODE_HOOK_EVENTS {
        let action = hook
            .action
            .map(crate::agent::IntegrationHookAction::as_str)
            .expect("Mastracode hook action should be present");
        let entries = hooks
            .get(hook.event)
            .and_then(Value::as_array)
            .expect("test precondition");
        assert_eq!(
            entries.len(),
            1,
            "{} should have one Shepr hook",
            hook.event
        );
        let command = entries[0]
            .get("command")
            .and_then(Value::as_str)
            .expect("test precondition");
        assert_eq!(
            command,
            mastracode_hook_command(&install_path(&installed, ArtifactRole::Hook), action)
        );
        assert_eq!(
            entries[0].get("type").and_then(Value::as_str),
            Some("command")
        );
        assert_eq!(
            entries[0].get("timeout").and_then(Value::as_u64),
            Some(MASTRACODE_HOOK_TIMEOUT_MS)
        );
    }
    assert_eq!(
        hooks["PostToolUse"][0]
            .get("command")
            .and_then(Value::as_str),
        Some("echo keep-me")
    );
}

fn grok_session_command(config: &Value) -> String {
    config["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("grok SessionStart command")
        .to_string()
}

#[test]
fn install_grok_writes_hook_and_config() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).expect("test precondition");
    env.set(GROK_CONFIG_DIR_TEST_SEAM, &grok_dir);

    let installed = install_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let hooks_dir = grok_dir.join("hooks");
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        hooks_dir.join(GROK_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::HookConfig),
        hooks_dir.join(GROK_HOOK_CONFIG_INSTALL_NAME)
    );
    assert_eq!(
        fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
            .expect("test precondition"),
        GROK_HOOK_ASSET
    );

    let config: Value = serde_json::from_str(
        &fs::read_to_string(install_path(&installed, ArtifactRole::HookConfig))
            .expect("test precondition"),
    )
    .expect("test precondition");
    assert_eq!(
        config,
        grok_hook_config(&install_path(&installed, ArtifactRole::Hook))
    );
    let session_start = config["hooks"]["SessionStart"]
        .as_array()
        .expect("test precondition");
    assert_eq!(session_start.len(), 1);
    let command = grok_session_command(&config);
    assert!(command.starts_with("sh "));
    assert!(command.contains("shepr-agent-state.sh"));
    assert!(command.ends_with(" session"));

    env.remove(GROK_CONFIG_DIR_TEST_SEAM);
}

#[test]
fn install_mastracode_is_idempotent_for_hook_entries() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    fs::create_dir_all(base.join(".mastracode")).expect("test precondition");
    env.set("HOME", &base);

    install_mastracode(&AgentIntegrationPaths::resolve()).expect("test precondition");
    install_mastracode(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(base.join(".mastracode").join("hooks.json"))
            .expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = hooks_file.as_object().expect("test precondition");
    for hook in MASTRACODE_HOOK_EVENTS {
        assert_eq!(
            hooks
                .get(hook.event)
                .and_then(Value::as_array)
                .expect("test precondition")
                .len(),
            1
        );
    }
}

#[test]
fn install_grok_is_idempotent() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).expect("test precondition");
    env.set(GROK_CONFIG_DIR_TEST_SEAM, &grok_dir);

    install_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let first = fs::read_to_string(grok_dir.join("hooks").join(GROK_HOOK_CONFIG_INSTALL_NAME))
        .expect("test precondition");
    install_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let second = fs::read_to_string(grok_dir.join("hooks").join(GROK_HOOK_CONFIG_INSTALL_NAME))
        .expect("test precondition");
    assert_eq!(first, second);

    env.remove(GROK_CONFIG_DIR_TEST_SEAM);
}

#[test]
fn install_mastracode_refuses_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    fs::create_dir_all(&base).expect("test precondition");
    env.set("HOME", &base);

    let err = install_mastracode(&AgentIntegrationPaths::resolve())
        .expect_err("missing mastracode directory must be refused")
        .to_string();

    assert!(
        err.contains("mastracode config directory not found"),
        "{err}"
    );
    assert!(!base.join(".mastracode").try_exists().expect("stat"));
}

#[test]
fn uninstall_mastracode_removes_shepr_hooks_and_preserves_others() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    fs::create_dir_all(base.join(".mastracode")).expect("test precondition");
    env.set("HOME", &base);

    install_mastracode(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let hooks_path = base.join(".mastracode").join("hooks.json");
    let mut hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(&hooks_path).expect("test precondition"))
            .expect("test precondition");
    hooks_file["UserPromptSubmit"]
        .as_array_mut()
        .expect("test precondition")
        .push(json!({ "type": "command", "command": "echo user-defined" }));
    fs::write(
        &hooks_path,
        serde_json::to_string_pretty(&hooks_file).expect("test precondition"),
    )
    .expect("test precondition");

    let result =
        uninstall_mastracode(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_updated(&result, ArtifactRole::Hooks));
    assert!(
        !base
            .join(".mastracode")
            .join("hooks")
            .join(MASTRACODE_HOOK_INSTALL_NAME)
            .stat_is_file()
    );

    let hooks_file: Value =
        serde_json::from_str(&fs::read_to_string(&hooks_path).expect("test precondition"))
            .expect("test precondition");
    let hooks = hooks_file.as_object().expect("test precondition");
    for hook in MASTRACODE_HOOK_EVENTS {
        if hook.event == "UserPromptSubmit" {
            continue;
        }
        assert!(
            !hooks.contains_key(hook.event),
            "{} should be removed",
            hook.event
        );
    }
    let user_prompt_submit = hooks
        .get("UserPromptSubmit")
        .and_then(Value::as_array)
        .expect("test precondition");
    assert_eq!(user_prompt_submit.len(), 1);
    assert_eq!(
        user_prompt_submit[0].get("command").and_then(Value::as_str),
        Some("echo user-defined")
    );
}

#[test]
fn install_grok_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    // Deliberately do not create the ~/.grok directory ahead of time: the
    // installer must refuse instead of conjuring a config dir for an agent
    // that is not installed.
    let missing = base.join(".grok");
    env.set(GROK_CONFIG_DIR_TEST_SEAM, &missing);

    let err = install_grok(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();
    assert!(
        err.contains("grok config directory not found"),
        "unexpected error: {err}"
    );

    env.remove(GROK_CONFIG_DIR_TEST_SEAM);
}

#[test]
fn uninstall_grok_removes_files() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).expect("test precondition");
    env.set(GROK_CONFIG_DIR_TEST_SEAM, &grok_dir);

    install_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let result = uninstall_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(uninstall_was_removed(&result, ArtifactRole::HookConfig));
    assert!(!uninstall_path(&result, ArtifactRole::Hook).stat_is_file());
    assert!(!uninstall_path(&result, ArtifactRole::HookConfig).stat_is_file());

    // Uninstalling again is a no-op.
    let again = uninstall_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(!uninstall_was_removed(&again, ArtifactRole::Hook));
    assert!(!uninstall_was_removed(&again, ArtifactRole::HookConfig));

    env.remove(GROK_CONFIG_DIR_TEST_SEAM);
}

#[test]
fn install_grok_uses_grok_config_dir_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let grok_dir = base.join("custom-grok");
    fs::create_dir_all(&grok_dir).expect("test precondition");
    env.set(GROK_CONFIG_DIR_TEST_SEAM, &grok_dir);

    let installed = install_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let hooks_dir = grok_dir.join("hooks");
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        hooks_dir.join(GROK_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::HookConfig),
        hooks_dir.join(GROK_HOOK_CONFIG_INSTALL_NAME)
    );

    clear_integration_path_env(&env);
}

#[test]
fn install_mastracode_errors_when_event_value_not_array() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let mastracode_dir = base.join(".mastracode");
    fs::create_dir_all(&mastracode_dir).expect("test precondition");
    fs::write(mastracode_dir.join("hooks.json"), r#"{"SessionStart":{}}"#)
        .expect("test precondition");
    env.set("HOME", &base);

    let err = install_mastracode(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();
    assert!(
        err.contains("hook entries for SessionStart must be an array"),
        "unexpected error: {err}"
    );
}

#[test]
fn uninstall_mastracode_errors_when_event_value_not_array() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let mastracode_dir = base.join(".mastracode");
    fs::create_dir_all(&mastracode_dir).expect("test precondition");
    fs::write(mastracode_dir.join("hooks.json"), r#"{"SessionStart":{}}"#)
        .expect("test precondition");
    env.set("HOME", &base);

    let err = uninstall_mastracode(&AgentIntegrationPaths::resolve())
        .expect_err("test precondition")
        .to_string();
    assert!(
        err.contains("hook entries for SessionStart must be an array"),
        "unexpected error: {err}"
    );
}

#[test]
fn install_antigravity_cli_writes_hook_and_updates_hooks_json() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).expect("test precondition");
    fs::write(
        agy_dir.join("hooks.json"),
        r#"{"lint-checker":{"PreInvocation":[{"type":"command","command":"echo keep-me"}]}}"#,
    )
    .expect("test precondition");
    env.set(EnvVar::AntigravityCliConfigDir, &agy_dir);

    let installed =
        install_antigravity_cli(&AgentIntegrationPaths::resolve()).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        agy_dir
            .join("hooks")
            .join(ANTIGRAVITY_CLI_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Hooks),
        agy_dir.join("hooks.json")
    );
    assert_eq!(
        fs::read_to_string(install_path(&installed, ArtifactRole::Hook))
            .expect("test precondition"),
        ANTIGRAVITY_CLI_HOOK_ASSET
    );

    let hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(agy_dir.join("hooks.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = hooks_file.as_object().expect("test precondition");

    // Shepr entries live under a named hook block; Antigravity CLI rejects a
    // file whose top level maps event names straight to arrays.
    let block = hooks
        .get(ANTIGRAVITY_CLI_HOOK_BLOCK_NAME)
        .and_then(Value::as_object)
        .expect("test precondition");

    for hook in ANTIGRAVITY_CLI_HOOK_EVENTS {
        let action = hook
            .action
            .map(crate::agent::IntegrationHookAction::as_str)
            .expect("Antigravity hook action should be present");
        let entries = block
            .get(hook.event)
            .and_then(Value::as_array)
            .expect("test precondition");
        assert_eq!(
            entries.len(),
            1,
            "{} should hold one Shepr entry",
            hook.event
        );
        let handler = &entries[0];

        // Handlers must be a flat list; the matcher/hooks wrapper is only
        // valid for tool events and invalidates the whole file here.
        assert!(
            handler.get("matcher").is_none() && handler.get("hooks").is_none(),
            "{} must be a flat handler, got {handler}",
            hook.event
        );

        assert_eq!(handler.get("type").and_then(Value::as_str), Some("command"));
        assert_eq!(
            handler.get("timeout").and_then(Value::as_u64),
            Some(ANTIGRAVITY_CLI_HOOK_TIMEOUT_SEC)
        );
        let command = handler
            .get("command")
            .and_then(Value::as_str)
            .expect("test precondition");
        assert_eq!(
            command,
            antigravity_cli_hook_command(&install_path(&installed, ArtifactRole::Hook), action)
        );
    }

    // The integration is session-only. Antigravity CLI cannot express blocked
    // state, skips PostInvocation on interruption, and fires Stop at end of
    // turn rather than process exit, so Shepr never claims lifecycle authority
    // here and screen detection owns agent state.
    for event in ["PreToolUse", "PostToolUse", "PostInvocation", "Stop"] {
        assert!(
            block.get(event).is_none(),
            "{event} must not be registered; lifecycle stays with screen detection"
        );
    }

    // Other named hooks are left untouched.
    assert_eq!(
        hooks
            .get("lint-checker")
            .and_then(|block| block.get("PreInvocation"))
            .and_then(Value::as_array)
            .and_then(|entries| entries.first())
            .and_then(|entry| entry.get("command"))
            .and_then(Value::as_str),
        Some("echo keep-me")
    );

    env.remove(EnvVar::AntigravityCliConfigDir);
}

#[test]
fn install_antigravity_cli_rewrites_stale_shepr_block() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).expect("test precondition");
    fs::write(
        agy_dir.join("hooks.json"),
        r#"{"shepr":{"Stop":[{"matcher":"*","hooks":[{"type":"command","command":"stale"}]}],"PostInvocation":[{"type":"command","command":"stale idle"}],"Legacy":[]}}"#,
    )
    .expect("test precondition");
    env.set(EnvVar::AntigravityCliConfigDir, &agy_dir);

    install_antigravity_cli(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(agy_dir.join("hooks.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let block = hooks_file
        .get(ANTIGRAVITY_CLI_HOOK_BLOCK_NAME)
        .and_then(Value::as_object)
        .expect("test precondition");

    // The block is Shepr-owned and rewritten wholesale rather than merged with.
    assert_eq!(
        block.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["PreInvocation"],
        "stale lifecycle events should be gone"
    );
    let entries = block
        .get("PreInvocation")
        .and_then(Value::as_array)
        .expect("test precondition");
    assert_eq!(entries.len(), 1);
    assert!(entries[0].get("hooks").is_none());
    assert!(
        entries[0]
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| command != "stale" && command != "stale idle")
    );

    env.remove(EnvVar::AntigravityCliConfigDir);
}

#[test]
fn install_antigravity_cli_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let agy_dir = base.join(".gemini").join("config");
    env.set(EnvVar::AntigravityCliConfigDir, &agy_dir);

    let err =
        install_antigravity_cli(&AgentIntegrationPaths::resolve()).expect_err("test precondition");
    assert!(err.to_string().contains("install antigravity cli first"));
    assert!(
        !agy_dir.try_exists().expect("stat"),
        "install must not create the config dir"
    );

    env.remove(EnvVar::AntigravityCliConfigDir);
}

#[test]
fn grok_integration_status_is_current_after_install() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).expect("test precondition");
    env.set(GROK_CONFIG_DIR_TEST_SEAM, &grok_dir);
    // A real install writes both the hook script and hooks/shepr.json.
    install_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");

    let statuses = installed_integration_statuses(&AgentIntegrationPaths::resolve());
    let grok = statuses
        .iter()
        .find(|status| status.target == crate::agent::IntegrationTarget::Grok)
        .expect("grok integration status");
    assert_eq!(grok.state, IntegrationStatusKind::Current);
    assert_eq!(grok.installed_version, Some(GROK_INTEGRATION_VERSION));

    clear_integration_path_env(&env);
}

#[test]
fn grok_status_reports_outdated_when_hook_config_missing_or_broken() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).expect("test precondition");
    env.set(GROK_CONFIG_DIR_TEST_SEAM, &grok_dir);
    install_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");
    let config_path = grok_dir.join("hooks").join(GROK_HOOK_CONFIG_INSTALL_NAME);

    let grok_state = || {
        installed_integration_statuses(&AgentIntegrationPaths::resolve())
            .into_iter()
            .find(|status| status.target == crate::agent::IntegrationTarget::Grok)
            .expect("grok integration status")
            .state
    };

    // Missing config: grok never runs the hook, so the install is not current.
    fs::remove_file(&config_path).expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Corrupt config.
    fs::write(&config_path, "{not json").expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Config that no longer references the hook script.
    fs::write(
        &config_path,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo other"}]}]}}"#,
    )
    .expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Config that mentions the script name without invoking it, and one that
    // invokes it without the required `session` action: both are
    // nonfunctional, so neither may report current.
    fs::write(
        &config_path,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo shepr-agent-state.sh"}]}]}}"#,
    )
    .expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);
    let hook_path = grok_dir.join("hooks").join(GROK_HOOK_INSTALL_NAME);
    fs::write(
        &config_path,
        format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"sh '{}'"}}]}}]}}}}"#,
            hook_path.display()
        ),
    )
    .expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Correct command but not a command-type hook: grok will not execute it.
    let session_command = grok_session_command(&grok_hook_config(&hook_path));
    fs::write(
        &config_path,
        format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"http","command":{}}}]}}]}}}}"#,
            serde_json::to_string(&session_command).expect("test precondition")
        ),
    )
    .expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // A matcher can prevent the expected hook from running.
    let mut config = grok_hook_config(&hook_path);
    config["hooks"]["SessionStart"][0]["matcher"] = json!("(");
    fs::write(
        &config_path,
        serde_json::to_string(&config).expect("test precondition"),
    )
    .expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // A malformed sibling group makes grok reject the event's hook groups.
    let mut config = grok_hook_config(&hook_path);
    config["hooks"]["SessionStart"]
        .as_array_mut()
        .expect("test precondition")
        .push(json!({}));
    fs::write(
        &config_path,
        serde_json::to_string(&config).expect("test precondition"),
    )
    .expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Reinstall repairs both files.
    install_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Current);

    clear_integration_path_env(&env);
}

#[test]
fn uninstall_antigravity_cli_removes_hooks_json_entries_and_hook_file() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let agy_dir = base.join(".gemini").join("config");
    fs::create_dir_all(&agy_dir).expect("test precondition");
    fs::write(
        agy_dir.join("hooks.json"),
        r#"{"lint-checker":{"PreInvocation":[{"type":"command","command":"echo keep-me"}]}}"#,
    )
    .expect("test precondition");
    env.set(EnvVar::AntigravityCliConfigDir, &agy_dir);

    // Install first
    let installed =
        install_antigravity_cli(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(install_path(&installed, ArtifactRole::Hook).stat_is_file());

    // Uninstall
    let result =
        uninstall_antigravity_cli(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert!(uninstall_was_removed(&result, ArtifactRole::Hook));
    assert!(!install_path(&installed, ArtifactRole::Hook).stat_is_file());
    assert!(uninstall_was_updated(&result, ArtifactRole::Hooks));

    let hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(agy_dir.join("hooks.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = hooks_file.as_object().expect("test precondition");

    // The Shepr block is gone and unrelated named hooks survive.
    assert!(hooks.get(ANTIGRAVITY_CLI_HOOK_BLOCK_NAME).is_none());
    assert!(hooks.contains_key("lint-checker"));

    env.remove(EnvVar::AntigravityCliConfigDir);
}

#[test]
fn grok_dir_honors_grok_home_after_config_dir_seam() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home_dir = base.join("grok-home");
    fs::create_dir_all(&home_dir).expect("test precondition");
    env.remove(GROK_CONFIG_DIR_TEST_SEAM);
    env.set(EnvVar::GrokHome, &home_dir);

    // The grok CLI reads its config (and hooks/) from $GROK_HOME, so the
    // integration must install there too.
    let installed = install_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        home_dir.join("hooks").join(GROK_HOOK_INSTALL_NAME)
    );

    // The shepr-level test seam still wins over GROK_HOME when set.
    let seam_dir = base.join("seam");
    fs::create_dir_all(&seam_dir).expect("test precondition");
    env.set(GROK_CONFIG_DIR_TEST_SEAM, &seam_dir);
    let installed = install_grok(&AgentIntegrationPaths::resolve()).expect("test precondition");
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        seam_dir.join("hooks").join(GROK_HOOK_INSTALL_NAME)
    );

    env.remove(EnvVar::GrokHome);
    clear_integration_path_env(&env);
}

#[test]
fn kimi_block_without_end_marker_is_refused_instead_of_truncating_the_file() {
    let damaged = format!(
        "model = \"k2\"\n\n{KIMI_CONFIG_BLOCK_BEGIN}\n[[hooks]]\nevent = \"Stop\"\n\n[user]\nkeep = true\n"
    );
    assert!(remove_kimi_config_block(&damaged).is_err());
    assert!(build_kimi_config_with_hooks(&damaged, Path::new("/hooks/x.sh")).is_err());

    let doubled = format!(
        "{KIMI_CONFIG_BLOCK_BEGIN}\na = 1\n{KIMI_CONFIG_BLOCK_BEGIN}\nb = 2\n{KIMI_CONFIG_BLOCK_END}\n"
    );
    assert!(remove_kimi_config_block(&doubled).is_err());

    let intact = format!(
        "model = \"k2\"\n\n{KIMI_CONFIG_BLOCK_BEGIN}\nx = 1\n{KIMI_CONFIG_BLOCK_END}\n\n[user]\nkeep = true\n"
    );
    let removed = remove_kimi_config_block(&intact).expect("test precondition");
    assert!(removed.contains("model = \"k2\""));
    assert!(removed.contains("[user]\nkeep = true"));
    assert!(!removed.contains("x = 1"));
}

#[test]
fn install_kimi_leaves_a_damaged_config_and_no_hook() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let kimi_dir = base.join("kimi");
    fs::create_dir_all(&kimi_dir).expect("test precondition");
    let damaged = format!("{KIMI_CONFIG_BLOCK_BEGIN}\n[user]\nkeep = true\n");
    fs::write(kimi_dir.join("config.toml"), &damaged).expect("test precondition");
    env.set(EnvVar::KimiCodeHome, &kimi_dir);

    assert!(install_kimi(&AgentIntegrationPaths::resolve()).is_err());
    assert_eq!(
        fs::read_to_string(kimi_dir.join("config.toml")).expect("test precondition"),
        damaged
    );
    assert!(
        !kimi_dir
            .join("hooks")
            .join(KIMI_HOOK_INSTALL_NAME)
            .try_exists()
            .expect("stat")
    );

    clear_integration_path_env(&env);
}

#[test]
fn codex_features_hooks_follow_the_users_features_shape() {
    fn parses(config: &str) -> toml::Value {
        toml::from_str(config).unwrap_or_else(|err| panic!("invalid toml {config:?}: {err}"))
    }
    fn hooks_enabled(config: &str) -> bool {
        parses(config)
            .get("features")
            .and_then(|features| features.get("hooks"))
            .and_then(toml::Value::as_bool)
            == Some(true)
    }

    // No features at all: a table is appended.
    let built = build_codex_config_with_hooks("model = \"o3\"\n").expect("test precondition");
    assert!(hooks_enabled(&built), "{built}");

    // Existing [features] table: key inserted under it, deprecated key dropped.
    let built = build_codex_config_with_hooks(
        "model = \"o3\"\n\n[features]\ncodex_hooks = true\nother = 1\n\n[tui]\nx = 1\n",
    )
    .expect("test precondition");
    assert!(hooks_enabled(&built), "{built}");
    assert!(!built.contains("codex_hooks"));

    // Root-level dotted keys: no second [features] table.
    for config in [
        "features.web_search = true\nmodel = \"o3\"\n",
        "features.hooks = false\n",
        "features . hooks=false\nfeatures.codex_hooks = true\n",
        "features.codex_hooks = true\n[tui]\nx = 1\n",
    ] {
        let built = build_codex_config_with_hooks(config).expect("test precondition");
        assert!(hooks_enabled(&built), "{config:?} -> {built}");
        assert!(!built.contains("[features]"), "{config:?} -> {built}");
        assert!(!built.contains("codex_hooks"), "{config:?} -> {built}");
    }

    // A dotted `features.*` key inside another table is not the root table.
    let built = build_codex_config_with_hooks("[profiles.x]\nfeatures.hooks = false\n")
        .expect("test precondition");
    assert!(hooks_enabled(&built), "{built}");

    // Inline tables are refused rather than broken.
    assert!(build_codex_config_with_hooks("features = { web_search = true }\n").is_err());
}

fn status_of(target: crate::agent::IntegrationTarget) -> IntegrationStatusKind {
    integration_status_rows(&AgentIntegrationPaths::resolve())
        .into_iter()
        .find_map(|row| row.ok().filter(|status| status.target == target))
        .expect("the target has a status row")
        .state
}

/// The operator messages of an install and uninstall round trip, one target
/// per artifact wording, and the status the round trip leaves behind.
#[test]
fn install_and_uninstall_messages_name_every_artifact() {
    use crate::agent::IntegrationTarget as Target;

    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    for dir in [".claude", ".pi/agent/extensions", ".letta", ".factory"] {
        fs::create_dir_all(home.join(dir)).expect("test precondition");
    }
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).expect("test precondition");
    env.set("HOME", &home);
    env.set(GROK_CONFIG_DIR_TEST_SEAM, &grok_dir);
    let paths = AgentIntegrationPaths::resolve();
    let claude = home.join(".claude");
    let pi = home
        .join(".pi/agent/extensions")
        .join(PI_EXTENSION_INSTALL_NAME);
    let letta = home.join(".letta");
    let droid = home.join(".factory");
    let grok = grok_dir.join("hooks");
    let shown = |path: PathBuf| path.display().to_string();

    let cases = [
        (
            Target::Claude,
            vec![
                format!(
                    "installed claude integration hook to {}",
                    shown(claude.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME))
                ),
                format!(
                    "ensured claude settings at {}",
                    shown(claude.join("settings.json"))
                ),
            ],
            vec![
                format!(
                    "removed claude hook at {}",
                    shown(claude.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME))
                ),
                format!(
                    "removed shepr claude hook entries from {}",
                    shown(claude.join("settings.json"))
                ),
            ],
            vec![
                format!(
                    "no claude hook found at {}",
                    shown(claude.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME))
                ),
                format!(
                    "no shepr claude hook entries found in {}",
                    shown(claude.join("settings.json"))
                ),
            ],
        ),
        (
            Target::Pi,
            vec![format!("installed pi integration to {}", shown(pi.clone()))],
            vec![format!(
                "removed pi integration extension at {}",
                shown(pi.clone())
            )],
            vec![format!(
                "no pi integration extension found at {}",
                shown(pi.clone())
            )],
        ),
        (
            Target::Letta,
            vec![
                format!(
                    "installed letta integration hook to {}",
                    shown(letta.join("hooks").join(LETTA_HOOK_INSTALL_NAME))
                ),
                format!(
                    "ensured letta settings at {}",
                    shown(letta.join("settings.json"))
                ),
            ],
            vec![
                format!(
                    "removed letta hook at {}",
                    shown(letta.join("hooks").join(LETTA_HOOK_INSTALL_NAME))
                ),
                format!(
                    "removed shepr letta hook entry from {}",
                    shown(letta.join("settings.json"))
                ),
            ],
            vec![
                format!(
                    "no letta hook found at {}",
                    shown(letta.join("hooks").join(LETTA_HOOK_INSTALL_NAME))
                ),
                format!(
                    "no shepr letta hook entry found in {}",
                    shown(letta.join("settings.json"))
                ),
            ],
        ),
        (
            Target::Droid,
            vec![
                format!(
                    "installed droid integration hook to {}",
                    shown(droid.join("hooks").join(DROID_HOOK_INSTALL_NAME))
                ),
                format!(
                    "ensured droid hooks at {}",
                    shown(droid.join("settings.json"))
                ),
            ],
            vec![
                format!(
                    "removed droid hook at {}",
                    shown(droid.join("hooks").join(DROID_HOOK_INSTALL_NAME))
                ),
                format!(
                    "removed shepr droid hook entries from {}",
                    shown(droid.join("settings.json"))
                ),
            ],
            vec![
                format!(
                    "no droid hook found at {}",
                    shown(droid.join("hooks").join(DROID_HOOK_INSTALL_NAME))
                ),
                format!(
                    "no shepr droid hook entries found in {}",
                    shown(droid.join("settings.json"))
                ),
            ],
        ),
        (
            Target::Grok,
            vec![
                format!(
                    "installed grok integration hook to {}",
                    shown(grok.join(GROK_HOOK_INSTALL_NAME))
                ),
                format!(
                    "registered grok hook config at {}",
                    shown(grok.join(GROK_HOOK_CONFIG_INSTALL_NAME))
                ),
            ],
            vec![
                format!(
                    "removed grok hook at {}",
                    shown(grok.join(GROK_HOOK_INSTALL_NAME))
                ),
                format!(
                    "removed grok hook config at {}",
                    shown(grok.join(GROK_HOOK_CONFIG_INSTALL_NAME))
                ),
            ],
            vec![
                format!(
                    "no grok hook found at {}",
                    shown(grok.join(GROK_HOOK_INSTALL_NAME))
                ),
                format!(
                    "no grok hook config found at {}",
                    shown(grok.join(GROK_HOOK_CONFIG_INSTALL_NAME))
                ),
            ],
        ),
    ];

    for (target, installed, removed, absent) in cases {
        assert_eq!(
            status_of(target),
            IntegrationStatusKind::NotInstalled,
            "{target:?}"
        );
        assert_eq!(
            install_target(&paths, target).expect("install succeeds"),
            installed,
            "{target:?}"
        );
        assert_eq!(
            status_of(target),
            IntegrationStatusKind::Current,
            "{target:?}"
        );
        assert_eq!(
            uninstall_target(&paths, target).expect("uninstall succeeds"),
            removed,
            "{target:?}"
        );
        assert_eq!(
            status_of(target),
            IntegrationStatusKind::NotInstalled,
            "{target:?}"
        );
        assert_eq!(
            uninstall_target(&paths, target).expect("a repeat uninstall succeeds"),
            absent,
            "{target:?}"
        );
    }

    clear_integration_path_env(&env);
}

/// The notices and the codex config line that only some targets print.
#[test]
fn install_and_uninstall_messages_keep_target_specific_lines() {
    let path = Path::new("/shepr-test/file");
    let outcome = InstallOutcome::default()
        .with_artifact(ArtifactRole::Config, path.to_path_buf())
        .with_notice(format!("requires kimi code {KIMI_MIN_VERSION} or newer"));
    let messages: Vec<String> = outcome
        .artifacts
        .iter()
        .map(|artifact| artifact.role.install_message("kimi", &artifact.path))
        .chain(outcome.notices.iter().cloned())
        .collect();
    assert_eq!(
        messages,
        vec![
            "ensured kimi config at /shepr-test/file".to_string(),
            format!("requires kimi code {KIMI_MIN_VERSION} or newer"),
        ]
    );
    assert_eq!(
        ArtifactRole::Config.uninstall_message("codex", path, UninstallState::Preserved),
        Some("left codex config unchanged at /shepr-test/file".to_string())
    );
    assert_eq!(
        ArtifactRole::TuiConfig.uninstall_message("opencode", path, UninstallState::Updated),
        Some("removed shepr opencode plugin entry from /shepr-test/file".to_string())
    );
    assert_eq!(
        ArtifactRole::UpdatedHooks.install_message("cursor", path),
        "updated cursor hooks at /shepr-test/file"
    );
}

/// `SHEPR_*` names the shipped assets spell that no shepr process reads or
/// writes into a child, so they stay outside the environment registry. Each
/// entry must still appear in some asset, or it is removed from this list.
const ASSET_INTERNAL_SHEPR_NAMES: &[&str] = &[
    // Header markers install and status code parse out of an asset's text
    // (`INTEGRATION_VERSION_MARKER`); they are not environment variables.
    "SHEPR_INTEGRATION_ID",
    "SHEPR_INTEGRATION_VERSION",
    // A hook script handing its arguments to the interpreter it runs.
    "SHEPR_ACTION",
    "SHEPR_HOOK_INPUT_FILE",
    "SHEPR_HOOK_SEQ",
    // Tunables only the omp extension reads.
    "SHEPR_OMP_IDLE_DEBOUNCE_MS",
    "SHEPR_OMP_RETRY_GRACE_MS",
    // The devin hook's injection seam for its own tests.
    "SHEPR_DEVIN_LIST_JSON",
];

fn collect_asset_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read an asset directory") {
        let path = entry.expect("read an asset directory entry").path();
        if path.stat_is_dir() {
            collect_asset_files(&path, files);
        } else {
            files.push(path);
        }
    }
}

/// The hook assets run inside other agents' runtimes and cannot link Rust
/// constants, so they restate the pane environment contract by name. Every
/// `SHEPR_*` name they spell must be one shepr owns (it reads the variable or
/// writes it into the pane) or a named asset-internal one, so a renamed or
/// removed variable cannot leave an asset reading a name nothing sets.
#[test]
fn every_shepr_name_in_the_shipped_assets_is_owned_or_asset_internal() {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/integration/assets");
    let mut files = Vec::new();
    collect_asset_files(&assets, &mut files);
    assert!(!files.is_empty(), "no assets under {}", assets.display());

    let owned: std::collections::BTreeSet<&str> = shepr_core::env::EnvVar::ALL
        .iter()
        .copied()
        .map(shepr_core::env::EnvVar::name)
        .chain(
            shepr_core::env::ChildEnv::ALL
                .iter()
                .copied()
                .map(shepr_core::env::ChildEnv::name),
        )
        .collect();
    for internal in ASSET_INTERNAL_SHEPR_NAMES {
        assert!(
            !owned.contains(internal),
            "{internal} is in the environment registry; drop it from the asset-internal list"
        );
    }
    assert_eq!(
        INTEGRATION_VERSION_MARKER.trim_end_matches('='),
        "SHEPR_INTEGRATION_VERSION"
    );

    let name = regex::Regex::new(r"SHEPR_[A-Z_]+").expect("test precondition");
    let mut seen = std::collections::BTreeSet::new();
    for file in &files {
        let text = String::from_utf8_lossy(&fs::read(file).expect("read an asset")).into_owned();
        for found in name.find_iter(&text) {
            let found = found.as_str().to_owned();
            assert!(
                owned.contains(found.as_str())
                    || ASSET_INTERNAL_SHEPR_NAMES.contains(&found.as_str()),
                "{} spells {found}, which no shepr process reads or writes into a pane; \
                 register it in shepr_core::env or list it as asset-internal here",
                file.display()
            );
            seen.insert(found);
        }
    }
    for internal in ASSET_INTERNAL_SHEPR_NAMES {
        assert!(
            seen.contains(*internal),
            "{internal} appears in no asset; drop it from the asset-internal list"
        );
    }
    // The pane contract every hook depends on is spelled by the assets.
    for contract in [
        shepr_core::env::EnvVar::SheprEnv.name(),
        shepr_core::env::EnvVar::SheprSocketPath.name(),
        shepr_core::env::EnvVar::SheprPaneId.name(),
        shepr_core::env::ChildEnv::SheprBinPath.name(),
    ] {
        assert!(seen.contains(contract), "no asset reads {contract}");
    }
}
