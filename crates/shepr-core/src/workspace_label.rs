use std::path::Path;

/// Choose a workspace label from an optional discovered Git root and the cwd.
pub fn workspace_label_from_cwd(cwd: &Path, repo_root: Option<&Path>) -> String {
    if repo_root.is_none() && std::env::var_os("HOME").as_deref() == Some(cwd.as_os_str()) {
        return "~".to_owned();
    }
    repo_root
        .and_then(Path::file_name)
        .or_else(|| cwd.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| cwd.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_label_uses_discovered_git_root() {
        assert_eq!(
            workspace_label_from_cwd(
                Path::new("/repos/example/nested"),
                Some(Path::new("/repos/example"))
            ),
            "example"
        );
    }
}
