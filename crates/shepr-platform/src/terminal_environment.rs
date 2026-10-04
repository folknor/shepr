use shepr_core::env::EnvVar;

/// Whether clipboard writes should travel through the host terminal.
pub(crate) fn prefers_osc52_clipboard() -> bool {
    prefers_osc52_clipboard_for_env(
        crate::env_present(EnvVar::SshConnection),
        crate::env_present(EnvVar::SshTty),
        crate::env_present(EnvVar::VscodeIpcHookCli),
    )
}

fn prefers_osc52_clipboard_for_env(
    ssh_connection: bool,
    ssh_tty: bool,
    vscode_ipc_hook_cli: bool,
) -> bool {
    ssh_connection || ssh_tty || vscode_ipc_hook_cli
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_sessions_prefer_osc52() {
        assert!(prefers_osc52_clipboard_for_env(true, false, false));
        assert!(prefers_osc52_clipboard_for_env(false, true, false));
        assert!(!prefers_osc52_clipboard_for_env(false, false, false));
    }

    #[test]
    fn vscode_remote_sessions_prefer_osc52() {
        assert!(prefers_osc52_clipboard_for_env(false, false, true));
    }
}
