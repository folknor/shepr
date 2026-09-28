use std::path::Path;

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn hook_command(hook_path: &Path, action: Option<&str>) -> String {
    hook_command_with_interpreter(hook_path, "bash", action)
}

pub(crate) fn hook_command_with_interpreter(
    hook_path: &Path,
    interpreter: &str,
    action: Option<&str>,
) -> String {
    let path = hook_path.display().to_string();
    let mut command = format!("{interpreter} {}", shell_single_quote(&path));
    if let Some(action) = action {
        command.push(' ');
        command.push_str(action);
    }
    command
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{hook_command, hook_command_with_interpreter};

    #[test]
    fn hook_command_formats_configured_interpreters_and_quotes_paths() {
        let hook_path = Path::new("/home/user/grok's hooks/shepr-agent-state.sh");
        let quoted_path = "'/home/user/grok'\"'\"'s hooks/shepr-agent-state.sh'";

        assert_eq!(
            hook_command_with_interpreter(hook_path, "sh", Some("session")),
            format!("sh {quoted_path} session")
        );
        assert_eq!(
            hook_command(hook_path, Some("session")),
            format!("bash {quoted_path} session")
        );
    }
}
