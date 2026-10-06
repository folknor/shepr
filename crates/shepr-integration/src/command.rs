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
    let relative_path = super::registry::primary_asset_path(target).join("/");
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
    use crate::env::AgentIntegrationPaths;
    use shepr_core::env::EnvVar;

    /// Targets whose registration holds a command; the others are loaded from
    /// a directory or by their own config and never build one.
    fn command_targets() -> impl Iterator<Item = IntegrationTarget> {
        IntegrationTarget::all().filter(|target| !target.hook_events().is_empty())
    }

    #[test]
    fn targets_without_a_registered_command_have_no_hook_events() {
        // `directory_setup` still has an arm for each of these only so that
        // its match stays exhaustive; none can reach `hook_command`.
        for target in [
            IntegrationTarget::Pi,
            IntegrationTarget::Omp,
            IntegrationTarget::Opencode,
            IntegrationTarget::Kilo,
        ] {
            assert!(target.hook_events().is_empty(), "{target:?}");
        }
    }

    /// Runs each registered command as an agent that hands it to a POSIX shell
    /// does (`sh -c`), against a stand-in hook that prints how it was reached.
    /// The command must resolve the same directory the installer wrote the
    /// asset into (parity with `env.rs`, whatever the override or `~` form)
    /// and pass the action through.
    #[test]
    fn registered_commands_run_under_sh_and_reach_the_installed_hook() {
        let overrides: [Option<&str>; 4] = [None, Some(""), Some("~/shell-parity"), Some("~")];
        for target in command_targets() {
            let override_var = target.agent().descriptor().config_dir_override;
            for xdg_config_home in [false, true] {
                for value in overrides {
                    let env = shepr_test_support::IsolatedEnv::new();
                    if xdg_config_home {
                        env.set(EnvVar::XdgConfigHome, env.path().join("xdg"));
                    }
                    // An absolute override is the one form with no HOME in it.
                    let absolute = env.path().join("absolute-override");
                    let value = match (value, override_var) {
                        (Some(value), Some(variable)) => {
                            env.set(variable, value);
                            Some(value)
                        }
                        (None, Some(variable)) if xdg_config_home => {
                            env.set(variable, &absolute);
                            None
                        }
                        _ => None,
                    };
                    let paths = AgentIntegrationPaths::resolve();
                    let hook = crate::registry::target_path(&paths, target).expect("hook path");
                    std::fs::create_dir_all(hook.parent().expect("hook parent"))
                        .expect("create hook directory");
                    std::fs::write(&hook, "printf '%s|%s' \"$0\" \"$1\"\n")
                        .expect("write stand-in hook");
                    let action = target
                        .hook_events()
                        .iter()
                        .find_map(|event| event.action)
                        .map(shepr_agent::IntegrationHookAction::as_str);
                    let command = hook_command(target, action);

                    // host-program-ok: the registered command is the subject, run as its agent runs it
                    let output = shepr_test_support::command_in_scratch("sh", "registered-command")
                        .arg("-c")
                        .arg(&command)
                        .output()
                        .expect("run sh");

                    let context =
                        format!("{target:?} override {value:?} xdg {xdg_config_home}: {command}");
                    assert!(output.status.success(), "{context}");
                    assert_eq!(
                        String::from_utf8_lossy(&output.stdout),
                        format!("{}|{}", hook.display(), action.unwrap_or("")),
                        "{context}"
                    );
                }
            }
        }
    }

    #[test]
    fn hook_command_resolves_the_config_directory_on_the_host_that_runs_it() {
        assert_eq!(
            hook_command(IntegrationTarget::Claude, Some("session")),
            "hook_dir=\"${CLAUDE_CONFIG_DIR:-$HOME/.claude}\"; case \"$hook_dir\" in \"~\") hook_dir=\"$HOME\" ;; \"~/\"*) hook_dir=\"$HOME/${hook_dir#\\~/}\" ;; esac; exec sh \"$hook_dir/hooks/shepr-agent-state.sh\" session"
        );
    }
}
