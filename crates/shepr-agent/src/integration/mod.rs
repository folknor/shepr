use std::time::Duration;

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

pub use actions::{install_target, uninstall_target};
pub use env::AgentIntegrationPaths;
pub use registry::{integration_status_rows, integration_target_label, outdated_update_notice};
pub use types::{
    InstallOutput, InstallWarning, IntegrationStatus, IntegrationStatusError, IntegrationStatusKind,
};

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
const COPILOT_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const COPILOT_HOOK_ASSET: &str = include_str!("assets/copilot/shepr-agent-state.sh");
const COPILOT_INTEGRATION_VERSION: u32 = 2;
const DEVIN_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const DEVIN_HOOK_ASSET: &str = include_str!("assets/devin/shepr-agent-state.sh");
const DEVIN_INTEGRATION_VERSION: u32 = 2;
const DROID_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const DROID_HOOK_ASSET: &str = include_str!("assets/droid/shepr-agent-state.sh");
const DROID_INTEGRATION_VERSION: u32 = 2;
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
const QODERCLI_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const QODERCLI_HOOK_ASSET: &str = include_str!("assets/qodercli/shepr-agent-state.sh");
const QODERCLI_INTEGRATION_VERSION: u32 = 1;
const QWEN_HOOK_INSTALL_NAME: &str = "shepr-agent-session.sh";
const QWEN_HOOK_ASSET: &str = include_str!("assets/qwen/shepr-agent-session.sh");
const QWEN_INTEGRATION_VERSION: u32 = 1;
const LETTA_HOOK_INSTALL_NAME: &str = "shepr-agent-session.sh";
const LETTA_HOOK_ASSET: &str = include_str!("assets/letta/shepr-agent-session.sh");
const LETTA_INTEGRATION_VERSION: u32 = 1;
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
const INTEGRATION_VERSION_MARKER: &str = "SHEPR_INTEGRATION_VERSION=";
const MASTRACODE_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const MASTRACODE_HOOK_ASSET: &str = include_str!("assets/mastracode/shepr-agent-state.sh");
const MASTRACODE_INTEGRATION_VERSION: u32 = 4;
const GROK_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const GROK_HOOK_ASSET: &str = include_str!("assets/grok/shepr-agent-state.sh");
const GROK_INTEGRATION_VERSION: u32 = 2;
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

// Hook assets deliver reports best-effort and discard failures, because the
// host agent may show a failing hook to the operator. The server refuses a
// report for a pane it does not know or with an empty agent label; the hook
// drops that refusal like any other failure.

// Each agent's own config files, named once. The `IntegrationSpec` rows list
// them for the registration check, and install and uninstall join the same
// constants, so the three can never spell a file differently.
const CLAUDE_SETTINGS_NAME: &str = "settings.json";
const CODEX_HOOKS_NAME: &str = "hooks.json";
const CODEX_CONFIG_NAME: &str = "config.toml";
const COPILOT_SETTINGS_NAME: &str = "settings.json";
const DEVIN_CONFIG_NAME: &str = "config.json";
const DROID_SETTINGS_NAME: &str = "settings.json";
const KIMI_CONFIG_NAME: &str = "config.toml";
const OPENCODE_TUI_CONFIG_NAME: &str = "tui.jsonc";
const OPENCODE_LEGACY_TUI_CONFIG_NAME: &str = "tui.json";
const OPENCODE_CLI_CONFIG_NAME: &str = "cli.json";
const QODERCLI_SETTINGS_NAME: &str = "settings.json";
const QWEN_SETTINGS_NAME: &str = "settings.json";
const LETTA_SETTINGS_NAME: &str = "settings.json";
const CURSOR_HOOKS_NAME: &str = "hooks.json";
const MASTRACODE_HOOKS_NAME: &str = "hooks.json";
const ANTIGRAVITY_CLI_HOOKS_NAME: &str = "hooks.json";
/// Lives in Grok's `hooks` directory beside the hook script, not in the
/// agent's config directory like the other names here.
const GROK_HOOK_CONFIG_NAME: &str = "shepr.json";

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
