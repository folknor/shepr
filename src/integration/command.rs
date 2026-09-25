use std::path::Path;

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn hook_command(hook_path: &Path, action: Option<&str>) -> String {
    let path = hook_path.display().to_string();
    let mut command = format!("bash {}", shell_single_quote(&path));
    if let Some(action) = action {
        command.push(' ');
        command.push_str(action);
    }
    command
}
