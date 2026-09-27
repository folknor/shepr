mod actions;
mod claude_settings;
mod command;
mod config_edit;
mod config_file;
mod env;
mod file_ops;
mod opencode_config;
mod registry;
mod targets;
mod types;
mod version;

pub(crate) use actions::{install_target, uninstall_target};
pub(crate) use env::AgentIntegrationPaths;
pub(crate) use registry::{
    installed_integration_statuses, integration_target_label, print_outdated_update_notice,
};
pub(crate) use types::{IntegrationStatus, IntegrationStatusKind};

const PI_EXTENSION_INSTALL_NAME: &str = "shepr-agent-state.ts";
const PI_EXTENSION_ASSET: &str = include_str!("assets/pi/shepr-agent-state.ts");
const PI_INTEGRATION_VERSION: u32 = 1;
const OMP_EXTENSION_INSTALL_NAME: &str = "shepr-omp-agent-state.ts";
const OMP_EXTENSION_ASSET: &str = include_str!("assets/omp/shepr-agent-state.ts");
const OMP_INTEGRATION_VERSION: u32 = 1;
const CLAUDE_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const CLAUDE_HOOK_ASSET: &str = include_str!("assets/claude/shepr-agent-state.sh");
const CLAUDE_INTEGRATION_VERSION: u32 = 2;
const CODEX_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const CODEX_HOOK_ASSET: &str = include_str!("assets/codex/shepr-agent-state.sh");
const CODEX_INTEGRATION_VERSION: u32 = 2;
const KIMI_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const KIMI_HOOK_ASSET: &str = include_str!("assets/kimi/shepr-agent-state.sh");
const KIMI_INTEGRATION_VERSION: u32 = 3;
const KIMI_CONFIG_BLOCK_BEGIN: &str = "# >>> shepr kimi integration";
const KIMI_CONFIG_BLOCK_END: &str = "# <<< shepr kimi integration";
const KIMI_MIN_VERSION: &str = "0.14.0";
const KIMI_HOOK_EVENTS: &[crate::agent::IntegrationHookEvent] =
    crate::agent::Agent::Kimi.integration_hook_events();
const COPILOT_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const COPILOT_HOOK_ASSET: &str = include_str!("assets/copilot/shepr-agent-state.sh");
const COPILOT_INTEGRATION_VERSION: u32 = 2;
const COPILOT_HOOK_EVENTS: &[crate::agent::IntegrationHookEvent] =
    crate::agent::Agent::GithubCopilot.integration_hook_events();
const DEVIN_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const DEVIN_HOOK_ASSET: &str = include_str!("assets/devin/shepr-agent-state.sh");
const DEVIN_INTEGRATION_VERSION: u32 = 2;
const DEVIN_HOOK_EVENTS: &[crate::agent::IntegrationHookEvent] =
    crate::agent::Agent::Devin.integration_hook_events();
const DROID_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const DROID_HOOK_ASSET: &str = include_str!("assets/droid/shepr-agent-state.sh");
const DROID_INTEGRATION_VERSION: u32 = 2;
const DROID_HOOK_EVENTS: &[crate::agent::IntegrationHookEvent] =
    crate::agent::Agent::Droid.integration_hook_events();
const OPENCODE_PLUGIN_INSTALL_NAME: &str = "shepr-agent-state.js";
const OPENCODE_PLUGIN_ASSET: &str = include_str!("assets/opencode/shepr-agent-state.js");
const OPENCODE_TUI_PLUGIN_INSTALL_NAME: &str = "shepr-tui-session.js";
const OPENCODE_TUI_PLUGIN_SPEC: &str = "./shepr-tui-session.js";
const OPENCODE_TUI_PLUGIN_ASSET: &str = include_str!("assets/opencode/shepr-tui-session.js");
const OPENCODE_V2_TUI_PLUGIN_DIR: &str = "shepr-opencode";
const OPENCODE_V2_TUI_PLUGIN_SPEC: &str = "./shepr-opencode";
const OPENCODE_V2_TUI_PLUGIN_ASSET: &str = include_str!("assets/opencode/tui.js");
const OPENCODE_INTEGRATION_VERSION: u32 = 1;
const KILO_PLUGIN_INSTALL_NAME: &str = "shepr-agent-state.js";
const KILO_PLUGIN_ASSET: &str = include_str!("assets/kilo/shepr-agent-state.js");
const KILO_INTEGRATION_VERSION: u32 = 2;
const HERMES_PLUGIN_INSTALL_NAME: &str = "shepr-agent-state";
const HERMES_PLUGIN_MANIFEST_INSTALL_NAME: &str = "plugin.yaml";
const HERMES_PLUGIN_INIT_INSTALL_NAME: &str = "__init__.py";
const HERMES_PLUGIN_MANIFEST_ASSET: &str = include_str!("assets/hermes/plugin.yaml");
const HERMES_PLUGIN_INIT_ASSET: &str = include_str!("assets/hermes/__init__.py");
const HERMES_INTEGRATION_VERSION: u32 = 1;
const QODERCLI_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const QODERCLI_HOOK_ASSET: &str = include_str!("assets/qodercli/shepr-agent-state.sh");
const QODERCLI_INTEGRATION_VERSION: u32 = 1;
const QODERCLI_HOOK_EVENTS: &[crate::agent::IntegrationHookEvent] =
    crate::agent::Agent::Qodercli.integration_hook_events();
const QWEN_HOOK_INSTALL_NAME: &str = "shepr-agent-session.sh";
const QWEN_HOOK_ASSET: &str = include_str!("assets/qwen/shepr-agent-session.sh");
const QWEN_INTEGRATION_VERSION: u32 = 1;
const QWEN_HOOK_EVENTS: &[crate::agent::IntegrationHookEvent] =
    crate::agent::Agent::Qwen.integration_hook_events();
const LETTA_HOOK_INSTALL_NAME: &str = "shepr-agent-session.sh";
const LETTA_HOOK_ASSET: &str = include_str!("assets/letta/shepr-agent-session.sh");
const LETTA_INTEGRATION_VERSION: u32 = 1;
const LETTA_HOOK_TIMEOUT_MS: u64 = 10_000;
const CURSOR_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const CURSOR_HOOK_ASSET: &str = include_str!("assets/cursor/shepr-agent-state.sh");
const CURSOR_INTEGRATION_VERSION: u32 = 2;
const ANTIGRAVITY_CLI_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const ANTIGRAVITY_CLI_HOOK_ASSET: &str =
    include_str!("assets/antigravity_cli/shepr-agent-state.sh");
const ANTIGRAVITY_CLI_INTEGRATION_VERSION: u32 = 1;
/// Antigravity CLI keys `hooks.json` by hook name, so every Shepr entry lives
/// under one Shepr-owned block that install rewrites and uninstall removes.
const ANTIGRAVITY_CLI_HOOK_BLOCK_NAME: &str = "shepr";
const ANTIGRAVITY_CLI_HOOK_TIMEOUT_SEC: u64 = 10;
const ANTIGRAVITY_CLI_HOOK_EVENTS: &[crate::agent::IntegrationHookEvent] =
    crate::agent::Agent::Antigravity.integration_hook_events();
const INTEGRATION_VERSION_MARKER: &str = "SHEPR_INTEGRATION_VERSION=";
const MASTRACODE_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const MASTRACODE_HOOK_ASSET: &str = include_str!("assets/mastracode/shepr-agent-state.sh");
const MASTRACODE_INTEGRATION_VERSION: u32 = 4;
const MASTRACODE_HOOK_TIMEOUT_MS: u64 = 10_000;
const MASTRACODE_HOOK_EVENTS: &[crate::agent::IntegrationHookEvent] =
    crate::agent::Agent::Mastracode.integration_hook_events();
const GROK_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const GROK_HOOK_CONFIG_INSTALL_NAME: &str = "shepr.json";
const GROK_HOOK_ASSET: &str = include_str!("assets/grok/shepr-agent-state.sh");
const GROK_INTEGRATION_VERSION: u32 = 2;

pub(crate) const INSTALL_WARNING_PREFIX: &str = "warning:";

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
