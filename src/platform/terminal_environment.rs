use std::ffi::OsStr;

/// Whether clipboard writes should travel through the host terminal.
pub(crate) fn prefers_osc52_clipboard() -> bool {
    prefers_osc52_clipboard_for_env(
        std::env::var_os("SSH_CONNECTION").as_deref(),
        std::env::var_os("SSH_TTY").as_deref(),
        std::env::var_os("VSCODE_IPC_HOOK_CLI").as_deref(),
        crate::platform::running_inside_wsl()
            || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists(),
    )
}

fn prefers_osc52_clipboard_for_env(
    ssh_connection: Option<&OsStr>,
    ssh_tty: Option<&OsStr>,
    vscode_ipc_hook_cli: Option<&OsStr>,
    wsl: bool,
) -> bool {
    ssh_connection.is_some() || ssh_tty.is_some() || vscode_ipc_hook_cli.is_some() || wsl
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_sessions_prefer_osc52() {
        assert!(prefers_osc52_clipboard_for_env(
            Some(OsStr::new("1 2 3 4")),
            None,
            None,
            false,
        ));
        assert!(prefers_osc52_clipboard_for_env(
            None,
            Some(OsStr::new("/dev/ttys001")),
            None,
            false,
        ));
        assert!(!prefers_osc52_clipboard_for_env(None, None, None, false));
    }

    #[test]
    fn wsl_and_vscode_remote_sessions_prefer_osc52() {
        assert!(prefers_osc52_clipboard_for_env(None, None, None, true));
        assert!(prefers_osc52_clipboard_for_env(
            None,
            None,
            Some(OsStr::new("/tmp/vscode-remote-cli.sock")),
            false,
        ));
    }
}
