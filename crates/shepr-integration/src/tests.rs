use super::actions::install_target;
use super::command::*;
use super::config_edit::*;
use super::env::*;
use super::registry::*;
use super::targets::*;
use super::types::*;
use super::*;
use crate::types::InstallResult;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use shepr_agent::{
    IntegrationTarget as Target, KIMI_ASK_USER_QUESTION_MATCHER, KIMI_OTHER_TOOL_MATCHER,
};
use shepr_core::env::EnvVar;
use shepr_test_support::IsolatedEnv;

use super::test_support::StatPath;

fn install_target_for_test(target: Target) -> InstallResult<InstallOutcome> {
    install_target_at_paths_for_test(target, &AgentIntegrationPaths::resolve())
}

fn install_target_at_paths_for_test(
    target: Target,
    paths: &AgentIntegrationPaths,
) -> InstallResult<InstallOutcome> {
    super::targets::install(paths, target)
}

fn install_path(outcome: &InstallOutcome, role: ArtifactRole) -> PathBuf {
    outcome
        .artifacts
        .iter()
        .find(|artifact| artifact.role == role)
        .expect("expected install artifact")
        .path
        .clone()
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
                && hook.get("timeout").and_then(toml::Value::as_integer)
                    == Some(i64::try_from(HOOK_TIMEOUT.as_secs()).expect("test precondition"))
        }),
        "missing kimi hook for {event} ({matcher:?}) -> {action}"
    );
}

/// The directory a test builds its fake homes and agent directories in,
/// inside the test's scratch directory.
fn unique_base(env: &IsolatedEnv) -> PathBuf {
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
        &install_target_for_test(Target::Pi).expect("test precondition"),
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
        &install_target_for_test(Target::Pi).expect("test precondition"),
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
        &install_target_for_test(Target::Pi).expect("test precondition"),
        ArtifactRole::Extension,
    )
    .clone();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));
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
        &install_target_for_test(Target::Pi).expect("test precondition"),
        ArtifactRole::Extension,
    )
    .clone();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));
}

#[test]
fn install_omp_writes_embedded_asset_to_omp_extensions_dir() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join(".omp/agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    env.set("HOME", &home);

    let installed = install_target_for_test(Target::Omp).expect("test precondition");
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
    env.set(EnvVar::PiConfigDir, "~/custom-omp");

    let installed = install_target_for_test(Target::Omp).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Extension),
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
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
    env.set(EnvVar::PiConfigDir, "~/ignored-omp-config");

    let installed = install_target_for_test(Target::Omp).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Extension),
        omp_dir.join("extensions").join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(pi_extension.stat_is_file());
    assert!(install_path(&installed, ArtifactRole::Extension).stat_is_file());
}

#[test]
fn install_omp_refuses_a_config_directory_shared_with_pi() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let shared_agent_dir = home.join(".pi/agent");
    fs::create_dir_all(&shared_agent_dir).expect("test precondition");
    env.set("HOME", &home);
    env.set(EnvVar::PiCodingAgentDir, &shared_agent_dir);
    env.set(EnvVar::PiConfigDir, "~/.pi");

    let error = install_target_for_test(Target::Omp)
        .expect_err("Pi and OMP cannot share one extension directory")
        .to_string();

    assert!(
        error.contains("pi and omp share integration directory"),
        "{error}"
    );
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

    let installed = install_target_for_test(Target::Omp).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Extension),
        ext_dir.join(OMP_EXTENSION_INSTALL_NAME)
    );
    assert!(ext_dir.stat_is_dir());
}

#[test]
fn install_omp_errors_when_extension_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_target_for_test(Target::Omp)
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("omp agent config directory not found"));
}

#[test]
fn a_drifted_asset_reads_outdated() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    let extension_path = ext_dir.join(PI_EXTENSION_INSTALL_NAME);
    fs::write(&extension_path, "// installed by shepr\n").expect("test precondition");
    env.set("HOME", &home);

    let status = integration_status(
        &AgentIntegrationPaths::resolve(),
        shepr_agent::IntegrationTarget::Pi,
    )
    .expect("status resolves");

    assert_eq!(status.state, IntegrationStatusKind::Outdated);
    assert_eq!(status.path, extension_path);
}

#[test]
fn the_exact_bundled_asset_reads_current() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).expect("test precondition");
    fs::write(ext_dir.join(PI_EXTENSION_INSTALL_NAME), PI_EXTENSION_ASSET)
        .expect("test precondition");
    env.set("HOME", &home);

    assert_eq!(
        status_of(shepr_agent::IntegrationTarget::Pi),
        IntegrationStatusKind::Current
    );
}

/// The server's launch install: present agents get their hooks, absent ones
/// are skipped without their directory being created, a current install is
/// left untouched, and a drifted one is repaired.
#[test]
fn launch_install_covers_present_agents_only() {
    use shepr_agent::IntegrationTarget as Target;

    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    let pi_agent_dir = home.join(".pi/agent");
    fs::create_dir_all(&claude_dir).expect("test precondition");
    fs::create_dir_all(&pi_agent_dir).expect("test precondition");
    env.set("HOME", &home);
    let paths = AgentIntegrationPaths::resolve();

    install_present_integrations(&paths);

    assert_eq!(status_of(Target::Claude), IntegrationStatusKind::Current);
    assert_eq!(status_of(Target::Pi), IntegrationStatusKind::Current);
    for absent in [Target::Codex, Target::Droid, Target::Cursor] {
        assert_eq!(
            status_of(absent),
            IntegrationStatusKind::NotInstalled,
            "{absent:?}"
        );
    }
    assert!(!home.join(".codex").stat_is_dir());
    assert!(!home.join(".factory").stat_is_dir());

    // A current install is not rewritten: the hook keeps its inode.
    let hook = claude_dir.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME);
    let inode = |path: &Path| {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).expect("stat hook").ino()
    };
    let before = inode(&hook);
    install_present_integrations(&paths);
    assert_eq!(inode(&hook), before);

    // A registration the user removed is put back.
    fs::write(claude_dir.join("settings.json"), "{}").expect("test precondition");
    assert_eq!(status_of(Target::Claude), IntegrationStatusKind::Outdated);
    install_present_integrations(&paths);
    assert_eq!(status_of(Target::Claude), IntegrationStatusKind::Current);
}

#[test]
fn launch_install_leaves_a_current_hook_alone_when_its_config_cannot_be_read() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).expect("test precondition");
    env.set("HOME", &home);
    let paths = AgentIntegrationPaths::resolve();
    let installed =
        install_target_at_paths_for_test(Target::Claude, &paths).expect("test precondition");
    let hook_path = install_path(&installed, ArtifactRole::Hook);
    let settings_path = claude_dir.join("settings.json");
    let broken_settings = "{ not json";
    fs::write(&settings_path, broken_settings).expect("test precondition");

    install_present_integrations(&paths);

    assert_eq!(
        fs::read_to_string(&settings_path).expect("settings remain readable"),
        broken_settings
    );
    assert_eq!(
        fs::read_to_string(hook_path).expect("hook remains readable"),
        CLAUDE_HOOK_ASSET
    );
}

/// One agent whose install fails does not stop the others.
#[test]
fn launch_install_failure_for_one_agent_leaves_the_others() {
    use shepr_agent::IntegrationTarget as Target;

    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    let droid_dir = home.join(".factory");
    fs::create_dir_all(&claude_dir).expect("test precondition");
    fs::create_dir_all(&droid_dir).expect("test precondition");
    fs::write(claude_dir.join("settings.json"), "{ not json").expect("test precondition");
    env.set("HOME", &home);

    install_present_integrations(&AgentIntegrationPaths::resolve());

    assert_eq!(
        status_of(Target::Claude),
        IntegrationStatusKind::NotInstalled
    );
    assert_eq!(status_of(Target::Droid), IntegrationStatusKind::Current);
}

#[test]
fn install_pi_errors_when_extension_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_target_for_test(Target::Pi)
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("pi agent config directory not found"));
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

    let installed = install_target_for_test(Target::Claude).expect("test precondition");
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

    let installed = install_target_for_test(Target::Claude).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Settings),
        claude_dir.join("settings.json")
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        claude_dir.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME)
    );
}

#[test]
fn claude_install_replaces_registrations_from_an_old_config_path_alias() {
    use std::os::unix::fs::symlink;

    use shepr_agent::IntegrationTarget as Target;

    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let shared_dir = base.join("shared-claude");
    let first_alias = base.join("host-a/claude");
    let second_alias = base.join("host-b/claude");
    fs::create_dir_all(&shared_dir).expect("test precondition");
    fs::create_dir_all(first_alias.parent().expect("test precondition"))
        .expect("test precondition");
    fs::create_dir_all(second_alias.parent().expect("test precondition"))
        .expect("test precondition");
    symlink(&shared_dir, &first_alias).expect("test precondition");
    symlink(&shared_dir, &second_alias).expect("test precondition");

    env.set(EnvVar::ClaudeConfigDir, &first_alias);
    install_target_for_test(Target::Claude).expect("first host install");
    assert_eq!(status_of(Target::Claude), IntegrationStatusKind::Current);

    env.set(EnvVar::ClaudeConfigDir, &second_alias);
    assert_eq!(status_of(Target::Claude), IntegrationStatusKind::Outdated);
    install_target_for_test(Target::Claude).expect("second host install");
    assert_eq!(status_of(Target::Claude), IntegrationStatusKind::Current);

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(shared_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");
    let session_start = settings["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart registrations");
    assert_eq!(session_start.len(), 1);
    let expected_command = hook_command(
        &second_alias.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME),
        Some("session"),
    );
    assert_eq!(
        session_start[0]["hooks"][0]["command"].as_str(),
        Some(expected_command.as_str())
    );
}

#[test]
fn install_claude_is_idempotent_for_hook_entries() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).expect("test precondition");
    env.set("HOME", &home);

    install_target_for_test(Target::Claude).expect("test precondition");
    install_target_for_test(Target::Claude).expect("test precondition");

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
fn install_claude_errors_when_claude_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_target_for_test(Target::Claude)
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("claude agent config directory not found"));
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

    let installed = install_target_for_test(Target::Codex).expect("test precondition");
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
    for (event, action) in [
        ("UserPromptSubmit", " working"),
        ("Stop", " idle"),
        ("Interrupt", " idle"),
    ] {
        assert!(
            hooks["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .expect("test precondition")
                .ends_with(action),
            "{event}"
        );
    }
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert!(config.contains("model = \"gpt-5.4\""));
    assert!(config.contains("[features]"));
    assert!(config.contains("hooks = true"));
}

#[test]
fn install_codex_uses_codex_home_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let codex_dir = base.join("custom-codex");
    fs::create_dir_all(&codex_dir).expect("test precondition");
    fs::write(codex_dir.join("config.toml"), "model = \"gpt-5.4\"\n").expect("test precondition");
    env.set(EnvVar::CodexHome, &codex_dir);

    let installed = install_target_for_test(Target::Codex).expect("test precondition");

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
}

#[test]
fn install_codex_is_idempotent_for_hook_entries_and_feature_flag() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).expect("test precondition");
    fs::write(codex_dir.join("config.toml"), "[features]\nother = true\n")
        .expect("test precondition");
    env.set("HOME", &home);

    install_target_for_test(Target::Codex).expect("test precondition");
    install_target_for_test(Target::Codex).expect("test precondition");

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
    for event in ["UserPromptSubmit", "Stop", "Interrupt"] {
        assert_eq!(
            hooks["hooks"][event]
                .as_array()
                .expect("test precondition")
                .len(),
            1,
            "{event}"
        );
    }
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert_eq!(config.matches("hooks = true").count(), 1);
    assert!(config.contains("other = true"));
}

#[test]
fn install_codex_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_target_for_test(Target::Codex)
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("codex agent config directory not found"));
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

    let installed = install_target_for_test(Target::Kimi).expect("test precondition");
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
    assert_eq!(
        hooks.len(),
        shepr_agent::Agent::Kimi.integration_hook_events().len() + 1
    );
    assert!(config.contains("default_model = \"moonshot\""));
    assert!(config.contains("command = \"echo keep\""));
    assert!(config.contains(KIMI_CONFIG_BLOCK_BEGIN));
    assert!(config.contains(KIMI_CONFIG_BLOCK_END));
    for hook in shepr_agent::Agent::Kimi.integration_hook_events() {
        let action = hook
            .action
            .map(shepr_agent::IntegrationHookAction::as_str)
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
        shepr_agent::Agent::Kimi
            .integration_hook_events()
            .iter()
            .any(|hook| {
                hook.event == event && hook.matcher == matcher && hook.action == Some(action)
            })
    };
    assert!(has_event(
        "PreToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        shepr_agent::IntegrationHookAction::Blocked,
    ));
    assert!(has_event(
        "PostToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        shepr_agent::IntegrationHookAction::Working,
    ));
    assert!(has_event(
        "PostToolUseFailure",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        shepr_agent::IntegrationHookAction::Working,
    ));
    assert!(has_event(
        "PreToolUse",
        Some(KIMI_OTHER_TOOL_MATCHER),
        shepr_agent::IntegrationHookAction::Working,
    ));
}

#[test]
fn install_kimi_uses_kimi_code_home_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let kimi_dir = base.join("custom-kimi");
    fs::create_dir_all(&kimi_dir).expect("test precondition");
    env.set(EnvVar::KimiCodeHome, &kimi_dir);

    let installed = install_target_for_test(Target::Kimi).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        kimi_dir.join("hooks").join(KIMI_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::Config),
        kimi_dir.join("config.toml")
    );
}

#[test]
fn install_kimi_is_idempotent_for_config_block() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).expect("test precondition");
    env.set("HOME", &home);

    install_target_for_test(Target::Kimi).expect("test precondition");
    install_target_for_test(Target::Kimi).expect("test precondition");

    let config = fs::read_to_string(kimi_dir.join("config.toml")).expect("test precondition");
    let hooks = kimi_config_hooks(&config);

    assert_eq!(config.matches(KIMI_CONFIG_BLOCK_BEGIN).count(), 1);
    assert_eq!(config.matches(KIMI_CONFIG_BLOCK_END).count(), 1);
    assert_eq!(
        hooks.len(),
        shepr_agent::Agent::Kimi.integration_hook_events().len()
    );
}

#[test]
fn install_kimi_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_target_for_test(Target::Kimi)
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("kimi agent config directory not found"));
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

    let installed = install_target_for_test(Target::Copilot).expect("test precondition");
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
        settings["hooks"]["SessionStart"][0]["bash"]
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

    let installed = install_target_for_test(Target::Copilot).expect("test precondition");
    install_target_for_test(Target::Copilot).expect("test precondition");

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
}

#[test]
fn install_copilot_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_target_for_test(Target::Copilot)
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("copilot agent config directory not found"));
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

    let installed = install_target_for_test(Target::Devin).expect("test precondition");
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
    for hook in shepr_agent::IntegrationTarget::Devin.hook_events() {
        let action = hook
            .action
            .map(shepr_agent::IntegrationHookAction::as_str)
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

    install_target_for_test(Target::Devin).expect("test precondition");
    install_target_for_test(Target::Devin).expect("test precondition");

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(devin_dir.join("config.json")).expect("test precondition"),
    )
    .expect("test precondition");
    for hook in shepr_agent::IntegrationTarget::Devin.hook_events() {
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
fn install_devin_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let xdg_config = base.join("xdg");
    fs::create_dir_all(&xdg_config).expect("test precondition");
    env.set("XDG_CONFIG_HOME", &xdg_config);
    env.set("HOME", base.join("home"));

    let err = install_target_for_test(Target::Devin)
        .expect_err("test precondition")
        .to_string();
    assert!(err.contains("devin agent config directory not found"));
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

    let installed = install_target_for_test(Target::Droid).expect("test precondition");
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
    for hook in shepr_agent::IntegrationTarget::Droid.hook_events() {
        let action = hook
            .action
            .map(shepr_agent::IntegrationHookAction::as_str)
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

    install_target_for_test(Target::Droid).expect("test precondition");
    install_target_for_test(Target::Droid).expect("test precondition");

    let settings: Value = serde_json::from_str(
        &fs::read_to_string(droid_dir.join("settings.json")).expect("test precondition"),
    )
    .expect("test precondition");
    for hook in shepr_agent::IntegrationTarget::Droid.hook_events() {
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
fn install_droid_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_target_for_test(Target::Droid)
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("droid agent config directory not found"));
}

#[test]
fn install_opencode_writes_server_and_tui_plugins() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).expect("test precondition");
    env.set("HOME", &home);

    let installed = install_target_for_test(Target::Opencode).expect("test precondition");

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
        let installed = install_target_for_test(Target::Opencode).expect("test precondition");
        assert_eq!(install_path(&installed, ArtifactRole::TuiConfig), json_path);

        assert!(!dir.join("tui.jsonc").try_exists().expect("stat"));
        assert_eq!(
            fs::read_to_string(&json_path).expect("test precondition"),
            original
        );
        assert_eq!(
            integration_status_at(
                shepr_agent::IntegrationTarget::Opencode,
                install_path(&installed, ArtifactRole::Plugin),
            )
            .expect("stat plugin")
            .state,
            IntegrationStatusKind::Current
        );
    }

    // OpenCode reads both files, so the plugin can be registered in each (a
    // hand-added entry beside shepr's); install reuses the jsonc one.
    let jsonc_path = dir.join("tui.jsonc");
    fs::write(
        &jsonc_path,
        r#"{"plugin":["./shepr-tui-session.js","another"]}"#,
    )
    .expect("test precondition");
    assert_eq!(
        install_path(
            &install_target_for_test(Target::Opencode).expect("test precondition"),
            ArtifactRole::TuiConfig
        ),
        jsonc_path
    );
    assert_eq!(
        fs::read_to_string(&json_path).expect("test precondition"),
        original
    );
    assert_eq!(fs::read_link(&dir).expect("test precondition"), dotfiles);
    fs::remove_dir_all(base).expect("test precondition");
}

#[test]
fn opencode_install_defers_v2_registration_while_migration_pending() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    let state_dir = home.join(".local/state/opencode");
    fs::create_dir_all(&opencode_dir).expect("test precondition");
    fs::create_dir_all(&state_dir).expect("test precondition");
    fs::write(state_dir.join("kv.json"), "{}").expect("test precondition");
    env.set("HOME", &home);

    let installed = install_target_for_test(Target::Opencode).expect("test precondition");

    assert!(!opencode_dir.join("cli.json").try_exists().expect("stat"));
    let status = || {
        integration_status_at(
            shepr_agent::IntegrationTarget::Opencode,
            install_path(&installed, ArtifactRole::Plugin).clone(),
        )
        .expect("status")
        .state
    };
    assert_eq!(status(), IntegrationStatusKind::Current);
    fs::remove_file(state_dir.join("kv.json")).expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Outdated);

    install_target_for_test(Target::Opencode).expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Current);
    assert!(opencode_dir.join("cli.json").stat_is_file());
    assert!(
        opencode_dir
            .join(OPENCODE_V2_TUI_PLUGIN_DIR)
            .join("tui.js")
            .stat_is_file()
    );
}

#[test]
fn opencode_v2_install_and_status_preserve_cli_preferences() {
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
    let installed = install_target_for_test(Target::Opencode).expect("test precondition");

    let status = || {
        integration_status_at(
            shepr_agent::IntegrationTarget::Opencode,
            install_path(&installed, ArtifactRole::Plugin).clone(),
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
    install_target_for_test(Target::Opencode).expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Current);
    // The user dropping shepr's cli.json entry reads outdated, and a reinstall
    // puts it back beside their own preferences.
    fs::write(
        &cli,
        r#"{"theme":{"name":"catppuccin"},"plugins":["other"]}"#,
    )
    .expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    install_target_for_test(Target::Opencode).expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Current);
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(&cli).expect("test precondition"))
            .expect("test precondition"),
        json!({"theme":{"name":"catppuccin"},"plugins":["other", OPENCODE_V2_TUI_PLUGIN_SPEC]})
    );
    fs::remove_file(&cli).expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    install_target_for_test(Target::Opencode).expect("test precondition");
    assert_eq!(status(), IntegrationStatusKind::Current);
}

#[test]
fn opencode_hard_link_rejection_precedes_install_asset_changes() {
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
    let target = shepr_agent::IntegrationTarget::Opencode;
    let error =
        install_target(&AgentIntegrationPaths::resolve(), target).expect_err("test precondition");
    assert!(error.to_string().contains("multiple hard links"));
    assert!(error.to_string().contains("cli.json"));
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
        install_target_for_test(Target::Opencode)
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
    let error = install_target_for_test(Target::Opencode).expect_err("test precondition");
    assert!(error.to_string().contains("multiple hard links"));
    assert!(error.to_string().contains("tui.json"));
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
    assert!(install_target_for_test(Target::Opencode).is_err());
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
    let installed = install_target_for_test(Target::Opencode).expect("test precondition");
    let status = || {
        integration_status_at(
            shepr_agent::IntegrationTarget::Opencode,
            install_path(&installed, ArtifactRole::Plugin).clone(),
        )
    };

    assert_eq!(
        status().expect("status").state,
        IntegrationStatusKind::Current
    );
    fs::remove_file(install_path(&installed, ArtifactRole::TuiPlugin)).expect("test precondition");
    assert_eq!(
        status().expect("status").state,
        IntegrationStatusKind::Outdated
    );
    fs::write(
        install_path(&installed, ArtifactRole::TuiPlugin),
        OPENCODE_TUI_PLUGIN_ASSET,
    )
    .expect("test precondition");
    assert_eq!(
        status().expect("status").state,
        IntegrationStatusKind::Current
    );
    fs::write(install_path(&installed, ArtifactRole::TuiConfig), "{}").expect("test precondition");
    assert_eq!(
        status().expect("status").state,
        IntegrationStatusKind::Outdated
    );
    fs::write(
        install_path(&installed, ArtifactRole::TuiConfig),
        "{ invalid json",
    )
    .expect("test precondition");
    let error = status().expect_err("invalid config must be reported");
    assert!(error.to_string().contains("failed to parse"));
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

    let err = install_target_for_test(Target::Opencode)
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
fn install_opencode_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_target_for_test(Target::Opencode)
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("opencode agent config directory not found"));
}

#[test]
fn install_kilo_writes_plugin_to_plugin_dir() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    let kilo_dir = home.join(".config/kilo");
    fs::create_dir_all(&kilo_dir).expect("test precondition");
    env.set("HOME", &home);

    let installed = install_target_for_test(Target::Kilo).expect("test precondition");
    let plugin_content = fs::read_to_string(install_path(&installed, ArtifactRole::Plugin))
        .expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Plugin),
        kilo_dir.join("plugin").join(KILO_PLUGIN_INSTALL_NAME)
    );
    assert_eq!(plugin_content, KILO_PLUGIN_ASSET);
}

#[test]
fn install_kilo_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    fs::create_dir_all(&home).expect("test precondition");
    env.set("HOME", &home);

    let err = install_target_for_test(Target::Kilo)
        .expect_err("test precondition")
        .to_string();

    assert!(err.contains("kilo agent config directory not found"));
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
        .find("function activateRootSession(ctx: any, sessionStartSource?: string): boolean")
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
        .find("pi.on(\"session_start\", (event, ctx)")
        .expect("omp extension registers session_start handler");
    let session_start_handler = &OMP_EXTENSION_ASSET[session_start..];
    session_start_handler
        .find("if (!activateRootSession(ctx, event?.reason || START.startup))")
        .expect("omp session_start handler should activate root session with its reason");

    // Per-turn activation must not claim a startup: only session_start and
    // session_switch select a session.
    let agent_start = omp_handler("agent_start");
    assert!(
        agent_start.contains("activateRootSession(ctx)"),
        "{agent_start}"
    );
    assert!(!agent_start.contains("START.startup"), "{agent_start}");

    let session_switch = OMP_EXTENSION_ASSET
        .find("pi.on(\"session_switch\", (event, ctx)")
        .expect("omp extension registers session_switch handler");
    let session_switch_handler = &OMP_EXTENSION_ASSET[session_switch..];
    session_switch_handler
        .find("if (!activateRootSession(ctx, event?.reason || START.resume))")
        .expect("omp session_switch handler should activate root session with switch reason");
}

#[test]
fn omp_session_reports_include_start_source() {
    let report_session = OMP_EXTENSION_ASSET
        .find("function reportSession(sessionStartSource?: string): Promise<void>")
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
        .find("requestQueue.then(")
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
        .find("activateBlocked();")
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
        .find("activateBlocked();")
        .expect("Ask start should block the pane");

    let ask_end_handler = omp_handler("tool_execution_end");
    ask_end_handler
        .find("event?.toolName !== \"ask\"")
        .expect("tool execution end should only treat Ask as blocked");
    ask_end_handler
        .find("deactivateBlocked();")
        .expect("Ask end should unblock the pane");
}

/// Runs the bundled Kimi hook with `payload` on stdin and returns the request
/// line it sent to a stand-in server socket, or `None` when it sent nothing.
/// The hook needs python3; callers check [`require_python3`] first.
fn run_kimi_hook(base: &Path, action: &str, payload: &[u8]) -> Option<String> {
    fs::create_dir_all(base).expect("test precondition");
    let hook = base.join(KIMI_HOOK_INSTALL_NAME);
    fs::write(&hook, KIMI_HOOK_ASSET).expect("test precondition");
    let socket_path = base.join("s.sock");

    // host-program-ok: the shipped hook asset is the subject, run as its agent runs it
    let mut command = shepr_test_support::command_in_scratch("sh", "kimi-hook");
    command.arg(&hook).arg(action).env(
        shepr_core::env::EnvVar::SheprBuildProfile.name(),
        shepr_paths::BuildProfile::Release.marker(),
    );
    let capture = shepr_test_support::capture_hook(command, &socket_path, base, "w1:p2", payload);
    assert!(
        capture.status.success(),
        "the hook must never fail its caller"
    );
    capture.requests.into_iter().next()
}

fn run_state_hook(
    base: &Path,
    name: &str,
    asset: &str,
    action: &str,
    payload: &[u8],
) -> (bool, Vec<u8>, Option<String>) {
    fs::create_dir_all(base).expect("test precondition");
    let hook = base.join("hook.sh");
    fs::write(&hook, asset).expect("test precondition");
    let socket_path = base.join("s.sock");

    // host-program-ok: the shipped shell hook asset is the subject, run as its agent runs it
    let mut command = shepr_test_support::command_in_scratch("sh", name);
    command.arg(&hook).arg(action).env(
        shepr_core::env::EnvVar::SheprBuildProfile.name(),
        shepr_paths::BuildProfile::Release.marker(),
    );
    let output = shepr_test_support::capture_hook(command, &socket_path, base, "w1:p2", payload);
    (
        output.status.success(),
        output.stderr,
        output.requests.into_iter().next(),
    )
}

/// Fails the test when python3 is missing, rather than letting it pass without
/// running: the python hook assets are the subject, and python3 is a
/// development dependency of shepr (the gate's script checks run on it too).
fn require_python3() {
    // host-program-ok: the shipped python hook assets are the subject
    let present = shepr_test_support::command_in_scratch("python3", "python3-probe")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(
        present,
        "python3 is not installed; it is a development dependency of shepr"
    );
}

#[test]
fn kimi_hook_reports_state_only_from_an_object_payload_naming_its_session() {
    let env = IsolatedEnv::new();
    require_python3();
    let base = unique_base(&env);

    // A payload that is not a JSON object names no session, and a state report
    // without its session cannot claim the pane: the hook sends nothing.
    let payloads: [&[u8]; 4] = [b"[1, 2]", b"\"text\"", b"null", b"not json"];
    for (index, payload) in payloads.into_iter().enumerate() {
        assert!(
            run_kimi_hook(&base.join(index.to_string()), "working", payload).is_none(),
            "payload {:?} sent a sessionless state report",
            String::from_utf8_lossy(payload)
        );
    }

    let request = run_kimi_hook(&base.join("object"), "working", br#"{"session_id":"abc"}"#)
        .expect("an object payload naming its session sends a report");
    let request: Value = serde_json::from_str(request.trim()).expect("test precondition");
    assert_eq!(request["method"], "pane.report_agent");
    assert_eq!(request["params"]["state"], "working");
    assert_eq!(request["params"]["pane_id"], "w1:p2");
    assert_eq!(request["params"]["agent_session_id"], "abc");
}

#[test]
fn session_required_state_hooks_match_the_descriptor_policy() {
    let env = IsolatedEnv::new();
    require_python3();
    let base = unique_base(&env);
    let agents: Vec<_> = shepr_agent::Agent::all()
        .filter(|agent| {
            agent
                .descriptor()
                .hook_session_policy()
                .state_requires_session_ref
        })
        .collect();
    assert_eq!(
        agents,
        vec![
            shepr_agent::Agent::Pi,
            shepr_agent::Agent::Codex,
            shepr_agent::Agent::Omp,
            shepr_agent::Agent::Mastracode,
            shepr_agent::Agent::OpenCode,
            shepr_agent::Agent::Kimi,
            shepr_agent::Agent::Kilo,
        ]
    );

    // Pi and OMP are TypeScript extensions; each checks the current ref before
    // its state request. OpenCode and Kilo have runtime tests beside their JS
    // assets. Execute the shell hook contracts here.
    for agent in [shepr_agent::Agent::Pi, shepr_agent::Agent::Omp] {
        let asset = integration_asset(
            agent
                .integration_target()
                .expect("session-required state reports have an integration"),
        )
        .expect("integration has a bundled asset");
        let Some(start) = asset.find("function sendState(") else {
            panic!("{} has no state sender", agent.label());
        };
        let send_state = &asset[start..];
        let guard = send_state
            .find("if (!currentSessionRef()) {")
            .unwrap_or_else(|| {
                panic!(
                    "{} can send state without its session reference",
                    agent.label()
                )
            });
        let send = send_state
            .find("return sendRequest(")
            .unwrap_or_else(|| panic!("{} has no state request", agent.label()));
        assert!(
            guard < send,
            "{} sends before checking its session reference",
            agent.label()
        );
    }

    for agent in [
        shepr_agent::Agent::Codex,
        shepr_agent::Agent::Kimi,
        shepr_agent::Agent::Mastracode,
    ] {
        let name = agent.label();
        let target = agent
            .integration_target()
            .expect("session-required state reports have an integration");
        let asset = integration_asset(target).expect("integration has a bundled asset");
        let (success, stderr, request) = run_state_hook(
            &base.join(format!("{name}-missing")),
            name,
            asset,
            "working",
            br#"{}"#,
        );
        assert!(success, "{name} hook failed without a session id");
        assert!(stderr.is_empty(), "{name} hook wrote to stderr");
        assert!(request.is_none(), "{name} sent a sessionless state report");

        let (success, stderr, request) = run_state_hook(
            &base.join(format!("{name}-present")),
            name,
            asset,
            "working",
            br#"{"session_id":"abc"}"#,
        );
        assert!(success, "{name} hook failed with a session id");
        assert!(stderr.is_empty(), "{name} hook wrote to stderr");
        let request = request.unwrap_or_else(|| panic!("{name} sent no state report"));
        let request: Value = serde_json::from_str(request.trim()).expect("test precondition");
        assert_eq!(request["method"], "pane.report_agent");
        assert_eq!(request["params"]["agent_session_id"], "abc");
    }
}

/// Runs a session-only python hook asset with `payload` on stdin. Returns the
/// hook's exit success, its stderr, and the request it sent to a stand-in
/// server socket (if any).
fn run_session_hook(base: &Path, asset: &str, payload: &[u8]) -> (bool, Vec<u8>, Option<String>) {
    fs::create_dir_all(base).expect("test precondition");
    let hook = base.join("hook.sh");
    fs::write(&hook, asset).expect("test precondition");
    let socket_path = base.join("s.sock");

    // host-program-ok: the shipped hook asset is the subject, run as its agent runs it
    let mut command = shepr_test_support::command_in_scratch("sh", "session-hook");
    command.arg(&hook).arg("session").env(
        shepr_core::env::EnvVar::SheprBuildProfile.name(),
        shepr_paths::BuildProfile::Release.marker(),
    );
    let output = shepr_test_support::capture_hook(command, &socket_path, base, "w1:p2", payload);
    (
        output.status.success(),
        output.stderr,
        output.requests.into_iter().next(),
    )
}

#[test]
fn session_hooks_ignore_non_object_payloads_quietly() {
    let env = IsolatedEnv::new();
    require_python3();
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

/// A Claude background session (`/fork`, `/bg`, agent view) or Claude's own
/// supervisor can carry the pane variables, but is never the pane's process:
/// the Claude hook reports nothing there, and still reports for any other
/// session kind.
#[test]
fn claude_hook_stays_silent_in_background_sessions() {
    let env = IsolatedEnv::new();
    require_python3();
    let base = unique_base(&env);
    let payload = br#"{"hook_event_name":"SessionStart","session_id":"abc","source":"fork"}"#;
    let cases: [(&str, &str, bool); 6] = [
        ("job-dir", "CLAUDE_JOB_DIR=/claude/jobs/abc", false),
        ("bg", "CLAUDE_CODE_SESSION_KIND=bg", false),
        ("daemon", "CLAUDE_CODE_SESSION_KIND=daemon", false),
        (
            "daemon-worker",
            "CLAUDE_CODE_SESSION_KIND=daemon-worker",
            false,
        ),
        ("other-kind", "CLAUDE_CODE_SESSION_KIND=interactive", true),
        ("empty-job-dir", "CLAUDE_JOB_DIR=", true),
    ];
    for (name, assignment, reports) in cases {
        let dir = base.join(name);
        fs::create_dir_all(&dir).expect("test precondition");
        let hook = dir.join("hook.sh");
        fs::write(&hook, CLAUDE_HOOK_ASSET).expect("test precondition");
        // `capture_hook` removes the Claude session variables a test run may
        // inherit, so the one under test is set by `env` inside the command.
        // host-program-ok: the shipped hook asset is the subject, run as its agent runs it
        let mut command = shepr_test_support::command_in_scratch("env", "claude-background-hook");
        command
            .arg(assignment)
            .arg("sh")
            .arg(&hook)
            .arg("session")
            .env(
                shepr_core::env::EnvVar::SheprBuildProfile.name(),
                shepr_paths::BuildProfile::Release.marker(),
            );
        let output =
            shepr_test_support::capture_hook(command, &dir.join("s.sock"), &dir, "w1:p2", payload);
        assert!(output.status.success(), "{name}: the hook failed");
        assert!(output.stderr.is_empty(), "{name}: the hook wrote to stderr");
        assert_eq!(!output.requests.is_empty(), reports, "{name}");
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

    let installed = install_target_for_test(Target::Cursor).expect("test precondition");

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

    install_target_for_test(Target::Cursor).expect("test precondition");
    install_target_for_test(Target::Cursor).expect("test precondition");

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
fn install_cursor_uses_cursor_config_dir_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let cursor_dir = base.join("custom-cursor");
    fs::create_dir_all(&cursor_dir).expect("test precondition");
    env.set(EnvVar::CursorConfigDir, &cursor_dir);

    let installed = install_target_for_test(Target::Cursor).expect("test precondition");

    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        cursor_dir.join(CURSOR_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::UpdatedHooks),
        cursor_dir.join("hooks.json")
    );
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
    install_target_for_test(Target::Cursor).expect("test precondition");

    let cursor = integration_status(
        &AgentIntegrationPaths::resolve(),
        shepr_agent::IntegrationTarget::Cursor,
    )
    .expect("cursor integration status");
    assert_eq!(cursor.state, IntegrationStatusKind::Current);
}

#[test]
fn cursor_required_version_is_added_and_checked_by_status() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let cursor_dir = base.join(".cursor");
    fs::create_dir_all(&cursor_dir).expect("test precondition");
    env.set(EnvVar::CursorConfigDir, &cursor_dir);

    install_target_for_test(Target::Cursor).expect("install Cursor hooks");
    let config_path = cursor_dir.join("hooks.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).expect("read config"))
        .expect("parse installed config");
    assert_eq!(config.get("version"), Some(&Value::from(1)));
    assert_eq!(
        integration_status(&AgentIntegrationPaths::resolve(), Target::Cursor)
            .expect("status")
            .state,
        IntegrationStatusKind::Current
    );

    config
        .as_object_mut()
        .expect("config object")
        .remove("version");
    fs::write(
        &config_path,
        serde_json::to_vec(&config).expect("serialize config"),
    )
    .expect("write config without required version");
    assert_eq!(
        integration_status(&AgentIntegrationPaths::resolve(), Target::Cursor)
            .expect("status missing Cursor version")
            .state,
        IntegrationStatusKind::Outdated
    );

    install_target_for_test(Target::Cursor).expect("restore required version");
    assert_eq!(
        integration_status(&AgentIntegrationPaths::resolve(), Target::Cursor)
            .expect("status repaired Cursor config")
            .state,
        IntegrationStatusKind::Current
    );
    env.remove(EnvVar::CursorConfigDir);
}

#[test]
fn install_cursor_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let missing = base.join(".cursor");
    env.set(EnvVar::CursorConfigDir, &missing);

    let err = install_target_for_test(Target::Cursor)
        .expect_err("test precondition")
        .to_string();
    assert!(
        err.contains("cursor agent config directory not found"),
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

    let installed = install_target_for_test(Target::Mastracode).expect("test precondition");

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
    for hook in shepr_agent::IntegrationTarget::Mastracode.hook_events() {
        let action = hook
            .action
            .map(shepr_agent::IntegrationHookAction::as_str)
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
            hook_command(&install_path(&installed, ArtifactRole::Hook), Some(action),)
        );
        assert_eq!(
            entries[0].get("type").and_then(Value::as_str),
            Some("command")
        );
        assert_eq!(
            entries[0].get("timeout").and_then(Value::as_u64),
            Some(u64::try_from(HOOK_TIMEOUT.as_millis()).expect("test precondition"))
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
    env.set(EnvVar::GrokHome, &grok_dir);

    let installed = install_target_for_test(Target::Grok).expect("test precondition");

    let hooks_dir = grok_dir.join("hooks");
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        hooks_dir.join(GROK_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::HookConfig),
        hooks_dir.join(GROK_HOOK_CONFIG_NAME)
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
        grok_hook_config(&install_path(&installed, ArtifactRole::Hook)).expect("test precondition")
    );
    let session_start = config["hooks"]["SessionStart"]
        .as_array()
        .expect("test precondition");
    assert_eq!(session_start.len(), 1);
    let command = grok_session_command(&config);
    assert!(command.starts_with("sh "));
    assert!(command.contains("shepr-agent-state.sh"));
    assert!(command.ends_with(" session"));
}

#[test]
fn install_mastracode_is_idempotent_for_hook_entries() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    fs::create_dir_all(base.join(".mastracode")).expect("test precondition");
    env.set("HOME", &base);

    install_target_for_test(Target::Mastracode).expect("test precondition");
    install_target_for_test(Target::Mastracode).expect("test precondition");

    let hooks_file: Value = serde_json::from_str(
        &fs::read_to_string(base.join(".mastracode").join("hooks.json"))
            .expect("test precondition"),
    )
    .expect("test precondition");
    let hooks = hooks_file.as_object().expect("test precondition");
    for hook in shepr_agent::IntegrationTarget::Mastracode.hook_events() {
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
    env.set(EnvVar::GrokHome, &grok_dir);

    install_target_for_test(Target::Grok).expect("test precondition");
    let first = fs::read_to_string(grok_dir.join("hooks").join(GROK_HOOK_CONFIG_NAME))
        .expect("test precondition");
    install_target_for_test(Target::Grok).expect("test precondition");
    let second = fs::read_to_string(grok_dir.join("hooks").join(GROK_HOOK_CONFIG_NAME))
        .expect("test precondition");
    assert_eq!(first, second);
}

#[test]
fn install_mastracode_refuses_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    fs::create_dir_all(&base).expect("test precondition");
    env.set("HOME", &base);

    let err = install_target_for_test(Target::Mastracode)
        .expect_err("missing mastracode directory must be refused")
        .to_string();

    assert!(
        err.contains("mastracode agent config directory not found"),
        "{err}"
    );
    assert!(!base.join(".mastracode").try_exists().expect("stat"));
}

#[test]
fn install_grok_errors_when_config_dir_missing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    // Deliberately do not create the ~/.grok directory ahead of time: the
    // installer must refuse instead of conjuring a config dir for an agent
    // that is not installed.
    let missing = base.join(".grok");
    env.set(EnvVar::GrokHome, &missing);

    let err = install_target_for_test(Target::Grok)
        .expect_err("test precondition")
        .to_string();
    assert!(
        err.contains("grok agent config directory not found"),
        "unexpected error: {err}"
    );
}

#[test]
fn install_grok_uses_grok_home_env() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let grok_dir = base.join("custom-grok");
    fs::create_dir_all(&grok_dir).expect("test precondition");
    env.set(EnvVar::GrokHome, &grok_dir);

    let installed = install_target_for_test(Target::Grok).expect("test precondition");

    let hooks_dir = grok_dir.join("hooks");
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        hooks_dir.join(GROK_HOOK_INSTALL_NAME)
    );
    assert_eq!(
        install_path(&installed, ArtifactRole::HookConfig),
        hooks_dir.join(GROK_HOOK_CONFIG_NAME)
    );
}

#[test]
fn hook_path_strip_rejects_non_array_event_values() {
    let error = super::json_edit::install_json(
        r#"{"hooks":{"UnrelatedEvent":{}}}"#,
        Path::new("/settings.json"),
        Path::new("/hooks/shepr-agent-state.sh"),
        super::registration::HooksRoot::HooksKey,
        serde_json::Map::new(),
        &[],
        "agent config",
    )
    .expect_err("all event values must be arrays");

    assert_eq!(
        error.to_string(),
        "hook entries for UnrelatedEvent must be an array"
    );
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

    let err = install_target_for_test(Target::Mastracode)
        .expect_err("test precondition")
        .to_string();
    assert!(
        err.contains("hook entries for SessionStart must be an array"),
        "unexpected error: {err}"
    );
}

#[test]
fn install_codex_rejects_non_array_event_while_stripping_hook_paths() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let codex_dir = base.join(".codex");
    fs::create_dir_all(&codex_dir).expect("test precondition");
    let hooks_path = codex_dir.join(CODEX_HOOKS_NAME);
    fs::write(&hooks_path, r#"{"hooks":{"UnrelatedEvent":{}}}"#).expect("test precondition");
    env.set("HOME", &base);

    let error = install_target_for_test(Target::Codex)
        .expect_err("malformed event list must be rejected during hook stripping")
        .to_string();

    assert!(
        error.contains("hook entries for UnrelatedEvent must be an array"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(&hooks_path).expect("test precondition"),
        r#"{"hooks":{"UnrelatedEvent":{}}}"#
    );
    assert!(
        !codex_dir
            .join(CODEX_HOOK_INSTALL_NAME)
            .try_exists()
            .expect("stat"),
        "refused installs must not leave a hook asset"
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

    let installed = install_target_for_test(Target::AntigravityCli).expect("test precondition");

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

    for hook in shepr_agent::Agent::Antigravity.integration_hook_events() {
        let action = hook
            .action
            .map(shepr_agent::IntegrationHookAction::as_str)
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
            Some(HOOK_TIMEOUT.as_secs())
        );
        let command = handler
            .get("command")
            .and_then(Value::as_str)
            .expect("test precondition");
        assert_eq!(
            command,
            hook_command(&install_path(&installed, ArtifactRole::Hook), Some(action),)
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

    install_target_for_test(Target::AntigravityCli).expect("test precondition");

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

    let err = install_target_for_test(Target::AntigravityCli).expect_err("test precondition");
    assert!(
        err.to_string()
            .contains("antigravity-cli agent config directory not found")
    );
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
    env.set(EnvVar::GrokHome, &grok_dir);
    // A real install writes both the hook script and hooks/shepr.json.
    install_target_for_test(Target::Grok).expect("test precondition");

    let grok = integration_status(
        &AgentIntegrationPaths::resolve(),
        shepr_agent::IntegrationTarget::Grok,
    )
    .expect("grok integration status");
    assert_eq!(grok.state, IntegrationStatusKind::Current);
}

#[test]
fn grok_status_distinguishes_missing_malformed_and_drifted_hook_config() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).expect("test precondition");
    env.set(EnvVar::GrokHome, &grok_dir);
    install_target_for_test(Target::Grok).expect("test precondition");
    let config_path = grok_dir.join("hooks").join(GROK_HOOK_CONFIG_NAME);

    let grok_status = || {
        integration_status(
            &AgentIntegrationPaths::resolve(),
            shepr_agent::IntegrationTarget::Grok,
        )
    };
    let grok_state = || grok_status().expect("grok status").state;

    // Missing config: grok never runs the hook, so the install is not current.
    fs::remove_file(&config_path).expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Corrupt config: the file is wholly shepr's, so it is drift the next
    // install replaces, not a user config error.
    fs::write(&config_path, "{not json").expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // Config that no longer references the hook script.
    fs::write(
        &config_path,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo other"}]}]}}"#,
    )
    .expect("test precondition");
    assert_eq!(
        grok_status().expect("status after valid config edit").state,
        IntegrationStatusKind::Outdated
    );

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
    let session_command =
        grok_session_command(&grok_hook_config(&hook_path).expect("test precondition"));
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
    let mut config = grok_hook_config(&hook_path).expect("test precondition");
    config["hooks"]["SessionStart"][0]["matcher"] = json!("(");
    fs::write(
        &config_path,
        serde_json::to_string(&config).expect("test precondition"),
    )
    .expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Outdated);

    // A malformed sibling group makes grok reject the event's hook groups.
    let mut config = grok_hook_config(&hook_path).expect("test precondition");
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
    install_target_for_test(Target::Grok).expect("test precondition");
    assert_eq!(grok_state(), IntegrationStatusKind::Current);
}

#[test]
fn grok_dir_honors_grok_home() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home_dir = base.join("grok-home");
    fs::create_dir_all(&home_dir).expect("test precondition");
    env.set(EnvVar::GrokHome, &home_dir);

    // The grok CLI reads its config (and hooks/) from $GROK_HOME, so the
    // integration must install there too.
    let installed = install_target_for_test(Target::Grok).expect("test precondition");
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        home_dir.join("hooks").join(GROK_HOOK_INSTALL_NAME)
    );

    // A changed GROK_HOME is captured by the next resolve call.
    let changed_home = base.join("changed-grok-home");
    fs::create_dir_all(&changed_home).expect("test precondition");
    env.set(EnvVar::GrokHome, &changed_home);
    let installed = install_target_for_test(Target::Grok).expect("test precondition");
    assert_eq!(
        install_path(&installed, ArtifactRole::Hook),
        changed_home.join("hooks").join(GROK_HOOK_INSTALL_NAME)
    );

    env.remove(EnvVar::GrokHome);
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
fn install_kimi_refuses_invalid_or_conflicting_config_before_writing() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let kimi_dir = base.join("kimi");
    fs::create_dir_all(&kimi_dir).expect("test precondition");
    env.set(EnvVar::KimiCodeHome, &kimi_dir);

    let config_path = kimi_dir.join("config.toml");
    let hook_path = kimi_dir.join("hooks").join(KIMI_HOOK_INSTALL_NAME);
    for config in ["model = [\n", "hooks = []\n", "[hooks]\ncustom = true\n"] {
        fs::write(&config_path, config).expect("test precondition");

        assert!(install_target_for_test(Target::Kimi).is_err());
        assert_eq!(
            fs::read_to_string(&config_path).expect("read config"),
            config
        );
        assert!(!hook_path.try_exists().expect("stat hook"));
    }
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

    assert!(install_target_for_test(Target::Kimi).is_err());
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

    // Existing [features] table: key inserted under it, the user's other keys
    // kept as written.
    let built = build_codex_config_with_hooks(
        "model = \"o3\"\n\n[features]\ncodex_hooks = true\nother = 1\n\n[tui]\nx = 1\n",
    )
    .expect("test precondition");
    assert!(hooks_enabled(&built), "{built}");
    assert!(built.contains("codex_hooks = true"), "{built}");

    // Root-level dotted keys: no second [features] table.
    for config in [
        "features.web_search = true\nmodel = \"o3\"\n",
        "features.codex_hooks = true\n[tui]\nx = 1\n",
    ] {
        let built = build_codex_config_with_hooks(config).expect("test precondition");
        assert!(hooks_enabled(&built), "{config:?} -> {built}");
        assert!(!built.contains("[features]"), "{config:?} -> {built}");
    }

    // The user's explicit opt-out is refused, however it is spelled.
    for config in [
        "features.hooks = false\n",
        "features . hooks=false\nfeatures.codex_hooks = true\n",
        "[features]\nhooks = false\n",
    ] {
        assert!(
            build_codex_config_with_hooks(config).is_err(),
            "{config:?} must be refused"
        );
    }

    // A dotted `features.*` key inside another table is not the root table.
    let built = build_codex_config_with_hooks("[profiles.x]\nfeatures.hooks = false\n")
        .expect("test precondition");
    assert!(hooks_enabled(&built), "{built}");

    // Inline tables are refused rather than broken.
    assert!(build_codex_config_with_hooks("features = { web_search = true }\n").is_err());
}

fn status_of(target: shepr_agent::IntegrationTarget) -> IntegrationStatusKind {
    integration_status(&AgentIntegrationPaths::resolve(), target)
        .expect("the target's status resolves")
        .state
}

/// An install reports every file it wrote, each with its role, and leaves the
/// integration current.
#[test]
fn install_outcomes_name_every_artifact() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    let home = base.join("home");
    for dir in [".claude", ".pi/agent/extensions", ".factory"] {
        fs::create_dir_all(home.join(dir)).expect("test precondition");
    }
    let grok_dir = base.join(".grok");
    fs::create_dir_all(&grok_dir).expect("test precondition");
    env.set("HOME", &home);
    env.set(EnvVar::GrokHome, &grok_dir);
    let paths = AgentIntegrationPaths::resolve();
    let claude = home.join(".claude");
    let droid = home.join(".factory");
    let grok = grok_dir.join("hooks");

    let cases = [
        (
            Target::Claude,
            vec![
                (
                    ArtifactRole::Hook,
                    claude.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME),
                ),
                (ArtifactRole::Settings, claude.join("settings.json")),
            ],
        ),
        (
            Target::Pi,
            vec![(
                ArtifactRole::Extension,
                home.join(".pi/agent/extensions")
                    .join(PI_EXTENSION_INSTALL_NAME),
            )],
        ),
        (
            Target::Droid,
            vec![
                (
                    ArtifactRole::Hook,
                    droid.join("hooks").join(DROID_HOOK_INSTALL_NAME),
                ),
                (ArtifactRole::Hooks, droid.join("settings.json")),
            ],
        ),
        (
            Target::Grok,
            vec![
                (ArtifactRole::Hook, grok.join(GROK_HOOK_INSTALL_NAME)),
                (ArtifactRole::HookConfig, grok.join(GROK_HOOK_CONFIG_NAME)),
            ],
        ),
    ];

    for (target, expected) in cases {
        assert_eq!(
            status_of(target),
            IntegrationStatusKind::NotInstalled,
            "{target:?}"
        );
        let output = install_target(&paths, target).expect("install succeeds");
        let artifacts = output
            .artifacts
            .into_iter()
            .map(|artifact| (artifact.role, artifact.path))
            .collect::<Vec<_>>();
        assert_eq!(artifacts, expected, "{target:?}");
        assert_eq!(
            status_of(target),
            IntegrationStatusKind::Current,
            "{target:?}"
        );
    }
}

/// `SHEPR_*` names the shipped assets spell that no shepr process reads or
/// writes into a child. Each entry must still appear in some asset, or it is
/// removed from the list.
const ASSET_INTERNAL_SHEPR_NAMES: &[&str] = shepr_core::env::SHEPR_ASSET_INTERNAL_NAMES;

pub(crate) fn collect_asset_files(dir: &Path, files: &mut Vec<PathBuf>) {
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
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/assets");
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
                 register it in shepr_core::env or list it in SHEPR_ASSET_INTERNAL_NAMES",
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
    ] {
        assert!(seen.contains(contract), "no asset reads {contract}");
    }
}

#[test]
fn installer_ignores_the_inherited_build_profile_marker() {
    let env = IsolatedEnv::new();
    let dir = env.home().join(".claude");
    fs::create_dir_all(&dir).expect("test precondition");
    env.set(
        shepr_core::env::EnvVar::SheprBuildProfile.name(),
        shepr_paths::BuildProfile::Dev.marker(),
    );
    install_present_integrations(&AgentIntegrationPaths::resolve());
    assert_eq!(
        status_of(shepr_agent::IntegrationTarget::Claude),
        IntegrationStatusKind::Current
    );
}

#[test]
fn shell_hooks_reject_dev_panes_after_draining_input() {
    let env = IsolatedEnv::new();
    let base = unique_base(&env);
    for target in shepr_agent::IntegrationTarget::all() {
        let asset = integration_asset(target).expect("target has an asset");
        // host-program-ok: tells the shipped shell hooks from the script-language ones
        if !asset.starts_with("#!/bin/sh") {
            continue;
        }
        let dir = base.join(target.label());
        fs::create_dir_all(&dir).expect("test precondition");
        let hook = dir.join("hook.sh");
        fs::write(&hook, asset).expect("test precondition");
        // host-program-ok: the shipped hook asset is the subject, run as its agent runs it
        let mut command = shepr_test_support::command_in_scratch("sh", "dev-hook");
        command.arg(&hook).arg("session").env(
            shepr_core::env::EnvVar::SheprBuildProfile.name(),
            shepr_paths::BuildProfile::Dev.marker(),
        );
        let capture = shepr_test_support::capture_hook(
            command,
            &dir.join("s.sock"),
            &dir,
            "w1:p2",
            br#"{"session_id":"dev-session"}"#,
        );
        assert!(capture.status.success(), "{target:?}");
        assert!(capture.stderr.is_empty(), "{target:?}");
        assert!(capture.requests.is_empty(), "{target:?}");
    }
}
