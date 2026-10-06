use std::path::Path;

use shepr_agent::IntegrationTarget as Target;

/// Resolve the agent config root in the environment of the host running this
/// command. Keep this aligned with the directory functions in `env.rs`.
fn directory_setup(target: Target) -> &'static str {
    match target {
        Target::Pi => {
            "hook_dir=\"${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}\"; hook_dir=\"$hook_dir/extensions\""
        }
        Target::Omp => {
            "hook_dir=${PI_CONFIG_DIR:-.omp}; case \"$hook_dir\" in /*) ;; \"~\") hook_dir=\"$HOME\" ;; \"~/\"*) hook_dir=\"$HOME/${hook_dir#\\~/}\" ;; *) hook_dir=\"$HOME/$hook_dir\" ;; esac; hook_dir=\"$hook_dir/agent/extensions\""
        }
        Target::Claude => "hook_dir=\"${CLAUDE_CONFIG_DIR:-$HOME/.claude}\"",
        Target::Codex => "hook_dir=\"${CODEX_HOME:-$HOME/.codex}\"",
        Target::Copilot => "hook_dir=\"${COPILOT_HOME:-$HOME/.copilot}\"",
        Target::Devin => "hook_dir=\"${XDG_CONFIG_HOME:-$HOME/.config}/devin\"",
        Target::Droid => "hook_dir=\"$HOME/.factory\"",
        Target::Kimi => "hook_dir=\"${KIMI_CODE_HOME:-$HOME/.kimi-code}\"",
        Target::Opencode => "hook_dir=\"${XDG_CONFIG_HOME:-$HOME/.config}/opencode\"",
        Target::Kilo => "hook_dir=\"${XDG_CONFIG_HOME:-$HOME/.config}/kilo\"",
        Target::Cursor => "hook_dir=\"${CURSOR_CONFIG_DIR:-$HOME/.cursor}\"",
        Target::Mastracode => "hook_dir=\"$HOME/.mastracode\"",
        Target::AntigravityCli => {
            "hook_dir=\"${ANTIGRAVITY_CLI_CONFIG_DIR:-$HOME/.gemini/config}\""
        }
        Target::Grok => "hook_dir=\"${GROK_HOME:-$HOME/.grok}\"",
    }
}

/// Build a command that resolves the hook under this host's agent config
/// directory. A config shared by several hosts therefore points at each
/// host's locally installed asset.
pub(crate) fn hook_command(target: Target, action: Option<&str>) -> String {
    let relative_path = super::registry::managed_assets(target)
        .next()
        .map_or_else(String::new, |asset| asset.path.join("/"));
    let mut command = format!(
        "{}; case \"$hook_dir\" in \"~\") hook_dir=\"$HOME\" ;; \"~/\"*) hook_dir=\"$HOME/${{hook_dir#\\~/}}\" ;; esac; exec sh \"$hook_dir/{relative_path}\"",
        directory_setup(target)
    );
    if let Some(action) = action {
        command.push(' ');
        command.push_str(&shepr_core::shell_quote::quote(action));
    }
    command
}

/// The absolute command format used by earlier installers, for recognizing
/// and repairing registrations written by a host before runtime resolution.
pub(crate) fn legacy_hook_command(hook_path: &Path, action: Option<&str>) -> String {
    let mut command = hook_command_prefix(hook_path);
    if let Some(action) = action {
        command.push(' ');
        command.push_str(&shepr_core::shell_quote::quote(action));
    }
    command
}

pub(crate) fn hook_command_prefix(hook_path: &Path) -> String {
    format!(
        "sh {}",
        shepr_core::shell_quote::quote_always(&hook_path.display().to_string())
    )
}

#[cfg(test)]
mod tests {
    use shepr_agent::IntegrationTarget;

    use super::hook_command;

    #[test]
    fn hook_command_resolves_the_config_directory_on_the_host_that_runs_it() {
        assert_eq!(
            hook_command(IntegrationTarget::Claude, Some("session")),
            "hook_dir=\"${CLAUDE_CONFIG_DIR:-$HOME/.claude}\"; case \"$hook_dir\" in \"~\") hook_dir=\"$HOME\" ;; \"~/\"*) hook_dir=\"$HOME/${hook_dir#\\~/}\" ;; esac; exec sh \"$hook_dir/hooks/shepr-agent-state.sh\" session"
        );
    }
}
