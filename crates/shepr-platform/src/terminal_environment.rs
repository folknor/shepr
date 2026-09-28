use shepr_core::env::EnvVar;

/// Whether clipboard writes should travel through the host terminal.
pub fn prefers_osc52_clipboard() -> bool {
    prefers_osc52_clipboard_for_env(
        crate::env_present(EnvVar::SshConnection),
        crate::env_present(EnvVar::SshTty),
        crate::env_present(EnvVar::VscodeIpcHookCli),
        crate::running_inside_wsl()
            || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists(),
    )
}

fn prefers_osc52_clipboard_for_env(
    ssh_connection: bool,
    ssh_tty: bool,
    vscode_ipc_hook_cli: bool,
    wsl: bool,
) -> bool {
    ssh_connection || ssh_tty || vscode_ipc_hook_cli || wsl
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_sessions_prefer_osc52() {
        assert!(prefers_osc52_clipboard_for_env(true, false, false, false));
        assert!(prefers_osc52_clipboard_for_env(false, true, false, false));
        assert!(!prefers_osc52_clipboard_for_env(false, false, false, false));
    }

    #[test]
    fn wsl_and_vscode_remote_sessions_prefer_osc52() {
        assert!(prefers_osc52_clipboard_for_env(false, false, false, true));
        assert!(prefers_osc52_clipboard_for_env(false, false, true, false));
    }
}
