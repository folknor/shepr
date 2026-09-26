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

pub(crate) use actions::{
    install_experimental_letta, install_target, uninstall_experimental_letta, uninstall_target,
};
#[cfg(test)]
pub(crate) use env::integration_env_lock;
pub(crate) use env::{
    SHEPR_PANE_ID_ENV_VAR, SHEPR_TAB_ID_ENV_VAR, SHEPR_WORKSPACE_ID_ENV_VAR, apply_pane_base_env,
};
pub(crate) use registry::{
    experimental_letta_integration_status, installed_integration_statuses,
    integration_target_label, print_outdated_update_notice,
};
pub(crate) use types::{ExperimentalIntegrationStatus, IntegrationStatus, IntegrationStatusKind};

/// CLI labels for experimental integrations that are not part of the
/// `IntegrationTarget` enum. Nothing on the wire keeps them out of it; they
/// are separate only until they are folded in.
pub(crate) const EXPERIMENTAL_INTEGRATION_TARGET_LABELS: &[&str] = &["letta"];

const PI_EXTENSION_INSTALL_NAME: &str = "shepr-agent-state.ts";
const PI_EXTENSION_ASSET: &str = include_str!("assets/pi/shepr-agent-state.ts");
const PI_INTEGRATION_VERSION: u32 = 1;
const OMP_EXTENSION_INSTALL_NAME: &str = "shepr-omp-agent-state.ts";
const OMP_EXTENSION_ASSET: &str = include_str!("assets/omp/shepr-agent-state.ts");
const OMP_INTEGRATION_VERSION: u32 = 1;
const CLAUDE_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const CLAUDE_HOOK_ASSET: &str = include_str!("assets/claude/shepr-agent-state.sh");
const CLAUDE_INTEGRATION_VERSION: u32 = 1;
const CODEX_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const CODEX_HOOK_ASSET: &str = include_str!("assets/codex/shepr-agent-state.sh");
const CODEX_INTEGRATION_VERSION: u32 = 1;
const KIMI_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const KIMI_HOOK_ASSET: &str = include_str!("assets/kimi/shepr-agent-state.sh");
const KIMI_INTEGRATION_VERSION: u32 = 3;
const KIMI_CONFIG_BLOCK_BEGIN: &str = "# >>> shepr kimi integration";
const KIMI_CONFIG_BLOCK_END: &str = "# <<< shepr kimi integration";
const KIMI_MIN_VERSION: &str = "0.14.0";
const KIMI_ASK_USER_QUESTION_MATCHER: &str = "^AskUserQuestion$";
const KIMI_OTHER_TOOL_MATCHER: &str = "^(?!AskUserQuestion$).*$";
const KIMI_HOOK_EVENTS: [(&str, Option<&str>, &str); 12] = [
    ("SessionStart", None, "session"),
    ("UserPromptSubmit", None, "working"),
    ("PreToolUse", Some(KIMI_OTHER_TOOL_MATCHER), "working"),
    (
        "PreToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "blocked",
    ),
    (
        "PostToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "working",
    ),
    (
        "PostToolUseFailure",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "working",
    ),
    ("SubagentStart", None, "working"),
    ("PreCompact", None, "working"),
    ("PermissionRequest", None, "blocked"),
    ("PermissionResult", None, "working"),
    ("Stop", None, "idle"),
    ("Interrupt", None, "idle"),
];
const COPILOT_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const COPILOT_HOOK_ASSET: &str = include_str!("assets/copilot/shepr-agent-state.sh");
const COPILOT_INTEGRATION_VERSION: u32 = 1;
const COPILOT_HOOK_EVENTS: [&str; 1] = ["SessionStart"];
const DEVIN_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const DEVIN_HOOK_ASSET: &str = include_str!("assets/devin/shepr-agent-state.sh");
const DEVIN_INTEGRATION_VERSION: u32 = 1;
const DEVIN_HOOK_EVENTS: [(&str, &str); 6] = [
    ("SessionStart", "session"),
    ("UserPromptSubmit", "session"),
    ("PreToolUse", "session"),
    ("PostToolUse", "session"),
    ("PermissionRequest", "session"),
    ("Stop", "session"),
];
const DROID_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const DROID_HOOK_ASSET: &str = include_str!("assets/droid/shepr-agent-state.sh");
const DROID_INTEGRATION_VERSION: u32 = 1;
const DROID_HOOK_EVENTS: [(&str, &str); 1] = [("SessionStart", "session")];
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
const QODERCLI_HOOK_EVENTS: [(&str, &str); 1] = [("SessionStart", "session")];
const QWEN_HOOK_INSTALL_NAME: &str = "shepr-agent-session.sh";
const QWEN_HOOK_ASSET: &str = include_str!("assets/qwen/shepr-agent-session.sh");
const QWEN_INTEGRATION_VERSION: u32 = 1;
const QWEN_HOOK_EVENTS: [(&str, &str); 1] = [("SessionStart", "session")];
const LETTA_HOOK_INSTALL_NAME: &str = "shepr-agent-session.sh";
const LETTA_HOOK_ASSET: &str = include_str!("assets/letta/shepr-agent-session.sh");
const LETTA_INTEGRATION_VERSION: u32 = 1;
const LETTA_HOOK_TIMEOUT_MS: u64 = 10_000;
const CURSOR_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const CURSOR_HOOK_ASSET: &str = include_str!("assets/cursor/shepr-agent-state.sh");
const CURSOR_INTEGRATION_VERSION: u32 = 1;
const ANTIGRAVITY_CLI_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const ANTIGRAVITY_CLI_HOOK_ASSET: &str =
    include_str!("assets/antigravity_cli/shepr-agent-state.sh");
const ANTIGRAVITY_CLI_INTEGRATION_VERSION: u32 = 1;
/// Antigravity CLI keys `hooks.json` by hook name, so every Shepr entry lives
/// under one Shepr-owned block that install rewrites and uninstall removes.
const ANTIGRAVITY_CLI_HOOK_BLOCK_NAME: &str = "shepr";
const ANTIGRAVITY_CLI_HOOK_TIMEOUT_SEC: u64 = 10;
/// `(event, reported action)`. Session-only: `PreInvocation` is the only event
/// we need because it carries `conversationId`. The others cannot express
/// lifecycle safely - Antigravity CLI has no blocked event, `PostInvocation` is
/// skipped on interruption, and `Stop` is end-of-turn rather than process exit.
/// Screen detection owns agent state instead.
///
/// `PreInvocation` takes a flat handler list; only the `PreToolUse`/`PostToolUse`
/// events accept a `matcher`/`hooks` wrapper, and sending one here would
/// invalidate the whole file.
const ANTIGRAVITY_CLI_HOOK_EVENTS: [(&str, &str); 1] = [("PreInvocation", "session")];
const INTEGRATION_VERSION_MARKER: &str = "SHEPR_INTEGRATION_VERSION=";
const MASTRACODE_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const MASTRACODE_HOOK_ASSET: &str = include_str!("assets/mastracode/shepr-agent-state.sh");
const MASTRACODE_INTEGRATION_VERSION: u32 = 3;
const MASTRACODE_HOOK_TIMEOUT_MS: u64 = 10_000;
const MASTRACODE_HOOK_EVENTS: [(&str, &str); 11] = [
    ("SessionStart", "session"),
    ("UserPromptSubmit", "working"),
    ("AgentStart", "working"),
    ("PreToolUse", "working"),
    ("PermissionRequest", "blocked"),
    ("PermissionResult", "working"),
    ("SubagentStart", "working"),
    ("SubagentEnd", "working"),
    ("Interrupt", "idle"),
    ("AgentEnd", "idle"),
    ("Stop", "idle"),
];
const GROK_HOOK_INSTALL_NAME: &str = "shepr-agent-state.sh";
const GROK_HOOK_CONFIG_INSTALL_NAME: &str = "shepr.json";
const GROK_HOOK_ASSET: &str = include_str!("assets/grok/shepr-agent-state.sh");
const GROK_INTEGRATION_VERSION: u32 = 1;

pub(crate) const INSTALL_WARNING_PREFIX: &str = "warning:";

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
