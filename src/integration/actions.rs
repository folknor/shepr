use std::io;

use super::KIMI_MIN_VERSION;
use super::env::AgentIntegrationPaths;
use super::registry::integration_target_label;
use super::targets::{
    install_antigravity_cli, install_claude, install_codex, install_copilot, install_cursor,
    install_devin, install_droid, install_grok, install_hermes, install_kilo, install_kimi,
    install_letta, install_mastracode, install_omp, install_opencode, install_pi, install_qodercli,
    install_qwen, uninstall_antigravity_cli, uninstall_claude, uninstall_codex, uninstall_copilot,
    uninstall_cursor, uninstall_devin, uninstall_droid, uninstall_grok, uninstall_hermes,
    uninstall_kilo, uninstall_kimi, uninstall_letta, uninstall_mastracode, uninstall_omp,
    uninstall_opencode, uninstall_pi, uninstall_qodercli, uninstall_qwen,
};
use super::version::{agent_version_requirement, enforce_agent_version};

pub(crate) fn install_target(
    paths: &AgentIntegrationPaths,
    target: crate::agents::IntegrationTarget,
) -> io::Result<Vec<String>> {
    let result = install_target_inner(paths, target);
    let outcome = if result.is_ok() { "ok" } else { "error" };
    crate::logging::integration_action("install", integration_target_label(target), outcome);
    result
}

fn install_target_inner(
    paths: &AgentIntegrationPaths,
    target: crate::agents::IntegrationTarget,
) -> io::Result<Vec<String>> {
    let version_warning = match agent_version_requirement(target) {
        Some(requirement) => enforce_agent_version(&requirement)?,
        None => None,
    };

    let mut messages = match target {
        crate::agents::IntegrationTarget::Pi => {
            let path = install_pi(paths)?;
            vec![format!("installed pi integration to {}", path.display())]
        }
        crate::agents::IntegrationTarget::Omp => {
            let installed = install_omp(paths)?;
            vec![format!(
                "installed omp integration to {}",
                installed.extension_path.display()
            )]
        }
        crate::agents::IntegrationTarget::Claude => {
            let installed = install_claude(paths)?;
            vec![
                format!(
                    "installed claude integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "ensured claude settings at {}",
                    installed.settings_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::Codex => {
            let installed = install_codex(paths)?;
            vec![
                format!(
                    "installed codex integration hook to {}",
                    installed.hook_path.display()
                ),
                format!("ensured codex hooks at {}", installed.hooks_path.display()),
                format!(
                    "ensured codex config at {}",
                    installed.config_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::Copilot => {
            let installed = install_copilot(paths)?;
            vec![
                format!(
                    "installed copilot integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "ensured copilot settings at {}",
                    installed.settings_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::Devin => {
            let installed = install_devin(paths)?;
            vec![
                format!(
                    "installed devin integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "ensured devin settings at {}",
                    installed.settings_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::Kimi => {
            let installed = install_kimi(paths)?;
            vec![
                format!(
                    "installed kimi integration hook to {}",
                    installed.hook_path.display()
                ),
                format!("ensured kimi config at {}", installed.config_path.display()),
                format!("requires kimi code {KIMI_MIN_VERSION} or newer"),
            ]
        }
        crate::agents::IntegrationTarget::Droid => {
            let installed = install_droid(paths)?;
            vec![
                format!(
                    "installed droid integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "ensured droid hooks at {}",
                    installed.settings_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::Opencode => {
            let installed = install_opencode(paths)?;
            let mut messages = vec![
                format!(
                    "installed opencode integration plugin to {}",
                    installed.plugin_path.display()
                ),
                format!(
                    "installed opencode tui integration plugin to {}",
                    installed.tui_plugin_path.display()
                ),
                format!(
                    "ensured opencode tui plugin config at {}",
                    installed.tui_config_path.display()
                ),
            ];
            if installed.cli_config_path.is_none() {
                messages.push(
                    "to enable OpenCode V2, start opencode2 once, then reinstall this integration"
                        .to_string(),
                );
            }
            messages
        }
        crate::agents::IntegrationTarget::Kilo => {
            let installed = install_kilo(paths)?;
            vec![format!(
                "installed kilo integration plugin to {}",
                installed.plugin_path.display()
            )]
        }
        crate::agents::IntegrationTarget::Hermes => {
            let installed = install_hermes(paths)?;
            vec![
                format!(
                    "installed hermes integration plugin to {}",
                    installed.plugin_dir.display()
                ),
                format!(
                    "enabled hermes plugin in {}",
                    installed.config_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::Qodercli => {
            let installed = install_qodercli(paths)?;
            vec![
                format!(
                    "installed qodercli integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "ensured qodercli settings at {}",
                    installed.settings_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::Qwen => {
            let installed = install_qwen(paths)?;
            vec![
                format!(
                    "installed qwen integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "ensured qwen settings at {}",
                    installed.settings_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::Cursor => {
            let installed = install_cursor(paths)?;
            vec![
                format!(
                    "installed cursor integration hook to {}",
                    installed.hook_path.display()
                ),
                format!("updated cursor hooks at {}", installed.hooks_path.display()),
            ]
        }
        crate::agents::IntegrationTarget::Mastracode => {
            let installed = install_mastracode(paths)?;
            vec![
                format!(
                    "installed mastracode integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "ensured mastracode hooks at {}",
                    installed.hooks_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::AntigravityCli => {
            let installed = install_antigravity_cli(paths)?;
            vec![
                format!(
                    "installed antigravity-cli integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "ensured antigravity-cli hooks at {}",
                    installed.hooks_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::Grok => {
            let installed = install_grok(paths)?;
            vec![
                format!(
                    "installed grok integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "registered grok hook config at {}",
                    installed.config_path.display()
                ),
            ]
        }
        crate::agents::IntegrationTarget::Letta => {
            let installed = install_letta(paths)?;
            vec![
                format!(
                    "installed letta integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "ensured letta settings at {}",
                    installed.settings_path.display()
                ),
            ]
        }
    };

    if let Some(warning) = version_warning {
        messages.push(warning);
    }

    Ok(messages)
}

pub(crate) fn uninstall_target(
    paths: &AgentIntegrationPaths,
    target: crate::agents::IntegrationTarget,
) -> io::Result<Vec<String>> {
    let result = uninstall_target_inner(paths, target);
    let outcome = if result.is_ok() { "ok" } else { "error" };
    crate::logging::integration_action("uninstall", integration_target_label(target), outcome);
    result
}

fn uninstall_target_inner(
    paths: &AgentIntegrationPaths,
    target: crate::agents::IntegrationTarget,
) -> io::Result<Vec<String>> {
    let messages = match target {
        crate::agents::IntegrationTarget::Pi => {
            let result = uninstall_pi(paths)?;
            if result.removed_extension {
                vec![format!(
                    "removed pi integration extension at {}",
                    result.extension_path.display()
                )]
            } else {
                vec![format!(
                    "no pi integration extension found at {}",
                    result.extension_path.display()
                )]
            }
        }
        crate::agents::IntegrationTarget::Omp => {
            let result = uninstall_omp(paths)?;
            if result.removed_extension {
                vec![format!(
                    "removed omp integration extension at {}",
                    result.extension_path.display()
                )]
            } else {
                vec![format!(
                    "no omp integration extension found at {}",
                    result.extension_path.display()
                )]
            }
        }
        crate::agents::IntegrationTarget::Claude => {
            let result = uninstall_claude(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed claude hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no claude hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_settings {
                messages.push(format!(
                    "removed shepr claude hook entries from {}",
                    result.settings_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr claude hook entries found in {}",
                    result.settings_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Codex => {
            let result = uninstall_codex(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed codex hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no codex hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_hooks {
                messages.push(format!(
                    "removed shepr codex hook entries from {}",
                    result.hooks_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr codex hook entries found in {}",
                    result.hooks_path.display()
                ));
            }
            messages.push(format!(
                "left codex config unchanged at {}",
                result.config_path.display()
            ));
            messages
        }
        crate::agents::IntegrationTarget::Copilot => {
            let result = uninstall_copilot(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed copilot hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no copilot hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_settings {
                messages.push(format!(
                    "removed shepr copilot hook entries from {}",
                    result.settings_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr copilot hook entries found in {}",
                    result.settings_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Devin => {
            let result = uninstall_devin(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed devin hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no devin hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_settings {
                messages.push(format!(
                    "removed shepr devin hook entries from {}",
                    result.settings_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr devin hook entries found in {}",
                    result.settings_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Kimi => {
            let result = uninstall_kimi(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed kimi hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no kimi hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_config {
                messages.push(format!(
                    "removed shepr kimi hook entries from {}",
                    result.config_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr kimi hook entries found in {}",
                    result.config_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Droid => {
            let result = uninstall_droid(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed droid hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no droid hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_settings {
                messages.push(format!(
                    "removed shepr droid hook entries from {}",
                    result.settings_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr droid hook entries found in {}",
                    result.settings_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Opencode => {
            let result = uninstall_opencode(paths)?;
            let mut messages = vec![if result.removed_plugin {
                format!(
                    "removed opencode integration plugin at {}",
                    result.plugin_path.display()
                )
            } else {
                format!(
                    "no opencode integration plugin found at {}",
                    result.plugin_path.display()
                )
            }];
            messages.push(if result.removed_tui_plugin {
                format!(
                    "removed opencode tui integration plugin at {}",
                    result.tui_plugin_path.display()
                )
            } else {
                format!(
                    "no opencode tui integration plugin found at {}",
                    result.tui_plugin_path.display()
                )
            });
            for path in result.updated_tui_configs {
                messages.push(format!(
                    "removed shepr opencode plugin entry from {}",
                    path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Kilo => {
            let result = uninstall_kilo(paths)?;
            if result.removed_plugin {
                vec![format!(
                    "removed kilo integration plugin at {}",
                    result.plugin_path.display()
                )]
            } else {
                vec![format!(
                    "no kilo integration plugin found at {}",
                    result.plugin_path.display()
                )]
            }
        }
        crate::agents::IntegrationTarget::Hermes => {
            let result = uninstall_hermes(paths)?;
            let mut messages = Vec::new();
            if result.removed_plugin_dir {
                messages.push(format!(
                    "removed hermes integration plugin at {}",
                    result.plugin_dir.display()
                ));
            } else {
                messages.push(format!(
                    "no hermes integration plugin found at {}",
                    result.plugin_dir.display()
                ));
            }
            if result.updated_config {
                messages.push(format!(
                    "disabled hermes plugin in {}",
                    result.config_path.display()
                ));
            } else {
                messages.push(format!(
                    "no hermes plugin entry found in {}",
                    result.config_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Qodercli => {
            let result = uninstall_qodercli(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed qodercli hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no qodercli hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_settings {
                messages.push(format!(
                    "removed shepr qodercli hook entries from {}",
                    result.settings_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr qodercli hook entries found in {}",
                    result.settings_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Qwen => {
            let result = uninstall_qwen(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed qwen hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no qwen hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_settings {
                messages.push(format!(
                    "removed shepr qwen hook entries from {}",
                    result.settings_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr qwen hook entries found in {}",
                    result.settings_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Cursor => {
            let result = uninstall_cursor(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed cursor hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no cursor hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_hooks {
                messages.push(format!(
                    "removed shepr cursor hook entries from {}",
                    result.hooks_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr cursor hook entries found in {}",
                    result.hooks_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Mastracode => {
            let result = uninstall_mastracode(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed mastracode hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no mastracode hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_hooks {
                messages.push(format!(
                    "removed shepr mastracode hook entries from {}",
                    result.hooks_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr mastracode hook entries found in {}",
                    result.hooks_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::AntigravityCli => {
            let result = uninstall_antigravity_cli(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed antigravity-cli hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no antigravity-cli hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_hooks {
                messages.push(format!(
                    "removed shepr antigravity-cli hook entries from {}",
                    result.hooks_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr antigravity-cli hook entries found in {}",
                    result.hooks_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Grok => {
            let result = uninstall_grok(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed grok hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no grok hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.removed_config_file {
                messages.push(format!(
                    "removed grok hook config at {}",
                    result.config_path.display()
                ));
            } else {
                messages.push(format!(
                    "no grok hook config found at {}",
                    result.config_path.display()
                ));
            }
            messages
        }
        crate::agents::IntegrationTarget::Letta => {
            let result = uninstall_letta(paths)?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed letta hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no letta hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_settings {
                messages.push(format!(
                    "removed shepr letta hook entry from {}",
                    result.settings_path.display()
                ));
            } else {
                messages.push(format!(
                    "no shepr letta hook entry found in {}",
                    result.settings_path.display()
                ));
            }
            messages
        }
    };

    Ok(messages)
}
