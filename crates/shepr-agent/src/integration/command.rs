use std::path::Path;

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn hook_command(hook_path: &Path, action: Option<&str>) -> String {
    let path = hook_path.display().to_string();
    let mut command = format!("sh {}", shell_single_quote(&path));
    if let Some(action) = action {
        command.push(' ');
        command.push_str(action);
    }
    command
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::hook_command;

    #[test]
    fn hook_command_uses_posix_shell_and_quotes_paths() {
        let hook_path = Path::new("/home/user/grok's hooks/shepr-agent-state.sh");
        let quoted_path = "'/home/user/grok'\"'\"'s hooks/shepr-agent-state.sh'";

        assert_eq!(
            hook_command(hook_path, Some("session")),
            format!("sh {quoted_path} session")
        );
    }
}
