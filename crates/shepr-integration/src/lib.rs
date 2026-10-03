//! Agent integration installer: the bundled hook assets and the edits that
//! register them in each agent's own config.

use crate::limits::HOOK_TIMEOUT;

mod actions;
mod atomic_replace;
mod claude_settings;
mod command;
mod config_edit;
mod config_file;
mod env;
mod file_ops;
mod limits;
mod logging;
mod opencode_config;
mod registration;
mod registry;
mod targets;
mod types;

pub use actions::install_present_integrations;
pub use env::AgentIntegrationPaths;
pub(crate) use types::{InstallErrorKind, IntegrationStatus, IntegrationStatusKind};

const PI_EXTENSION_INSTALL_NAME: &str = "shepr-agent-state.ts";
const PI_EXTENSION_ASSET: &str = include_str!("assets/pi/shepr-agent-state.ts");
const OMP_EXTENSION_INSTALL_NAME: &str = "shepr-omp-agent-state.ts";
const OMP_EXTENSION_ASSET: &str = include_str!("assets/omp/shepr-agent-state.ts");
const CLAUDE_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const CLAUDE_HOOK_ASSET: &str = include_str!("assets/claude/shepr-agent-state.sh");
const CODEX_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const CODEX_HOOK_ASSET: &str = include_str!("assets/codex/shepr-agent-state.sh");
const KIMI_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const KIMI_HOOK_ASSET: &str = include_str!("assets/kimi/shepr-agent-state.sh");
const KIMI_CONFIG_BLOCK_BEGIN: &str = "# >>> shepr kimi integration";
const KIMI_CONFIG_BLOCK_END: &str = "# <<< shepr kimi integration";
const COPILOT_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const COPILOT_HOOK_ASSET: &str = include_str!("assets/copilot/shepr-agent-state.sh");
const DEVIN_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const DEVIN_HOOK_ASSET: &str = include_str!("assets/devin/shepr-agent-state.sh");
const DROID_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const DROID_HOOK_ASSET: &str = include_str!("assets/droid/shepr-agent-state.sh");
const OPENCODE_PLUGIN_INSTALL_NAME: &str = "shepr-agent-state.js";
const OPENCODE_PLUGIN_ASSET: &str = include_str!("assets/opencode/shepr-agent-state.js");
const OPENCODE_TUI_PLUGIN_INSTALL_NAME: &str = "shepr-tui-session.js";
const OPENCODE_TUI_PLUGIN_SPEC: &str = "./shepr-tui-session.js";
const OPENCODE_TUI_PLUGIN_ASSET: &str = include_str!("assets/opencode/shepr-tui-session.js");
const OPENCODE_V2_TUI_PLUGIN_DIR: &str = "shepr-opencode";
const OPENCODE_V2_TUI_PLUGIN_SPEC: &str = "./shepr-opencode";
const OPENCODE_V2_TUI_PLUGIN_ASSET: &str = include_str!("assets/opencode/tui.js");
const KILO_PLUGIN_INSTALL_NAME: &str = "shepr-agent-state.js";
const KILO_PLUGIN_ASSET: &str = include_str!("assets/kilo/shepr-agent-state.js");
const CURSOR_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const CURSOR_HOOK_ASSET: &str = include_str!("assets/cursor/shepr-agent-state.sh");
const ANTIGRAVITY_CLI_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const ANTIGRAVITY_CLI_HOOK_ASSET: &str =
    include_str!("assets/antigravity_cli/shepr-agent-state.sh");
/// Antigravity CLI keys `hooks.json` by hook name, so every Shepr entry lives
/// under one Shepr-owned block that install rewrites.
const ANTIGRAVITY_CLI_HOOK_BLOCK_NAME: &str = "shepr";
// The shared asset-name audit accounts for this marker in the bundled hooks;
// it is diagnostic metadata and never determines whether an install is current.
const INTEGRATION_VERSION_MARKER: &str = "SHEPR_INTEGRATION_VERSION=";
const MASTRACODE_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const MASTRACODE_HOOK_ASSET: &str = include_str!("assets/mastracode/shepr-agent-state.sh");
const GROK_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const GROK_HOOK_ASSET: &str = include_str!("assets/grok/shepr-agent-state.sh");

// Hook assets deliver reports best-effort and discard failures, because the
// host agent may show a failing hook to the operator. The server refuses a
// report for a pane it does not know or with an empty agent label; the hook
// drops that refusal like any other failure.
//
// Every asset opens the API socket itself and builds one envelope shape: a
// single JSON request line whose `id` is `<source>:<seq>`, the report's own
// `source` and `seq` params (the OpenCode TUI's session selection report has
// no seq and puts a clock reading in the seq's unit there), and every socket
// attempt waits at most 500 ms for the reply. `hook_assets_share_one_envelope`
// in the tests holds the assets to it, and the server crate's
// `agent_integration_contract_tests` module replays each asset's scripted
// session into terminal state.
//
// Each installed asset is standalone and translates that agent's own
// lifecycle payload into the API request. Those payload decoders and
// lifecycle gates differ by agent, so generating the scripts from one template
// would add a build-time source path without removing that agent-specific
// logic. Hooks write directly to the server socket, so no reporter command
// belongs in the CLI.

// Each agent's own config files, named once. The `IntegrationSpec` rows list
// them for the registration check, and install joins the same constants, so
// the two can never spell a file differently.
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
