use std::path::Path;

/// Choose a workspace label from an optional discovered Git root and the cwd.
/// `home_dir` is supplied by the caller after resolving it with
/// `pathutil::home_dir`, keeping this helper independent of process state.
pub fn workspace_label_from_cwd(
    cwd: &Path,
    repo_root: Option<&Path>,
    home_dir: Option<&Path>,
) -> String {
    if repo_root.is_none() && home_dir.is_some_and(|home| home == cwd) {
        return "~".to_owned();
    }
    repo_root
        .and_then(Path::file_name)
        .or_else(|| cwd.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map_or_else(|| cwd.display().to_string(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_label_uses_discovered_git_root() {
        assert_eq!(
            workspace_label_from_cwd(
                Path::new("/repos/example/nested"),
                Some(Path::new("/repos/example")),
                None,
            ),
            "example"
        );
    }

    #[test]
    fn workspace_label_is_tilde_only_for_home_outside_git() {
        let home = Path::new("/home/user");
        assert_eq!(workspace_label_from_cwd(home, None, Some(home)), "~");
        assert_eq!(
            workspace_label_from_cwd(home, Some(home), Some(home)),
            "user"
        );
        assert_eq!(workspace_label_from_cwd(home, None, None), "user");
        assert_eq!(
            workspace_label_from_cwd(Path::new("/home/user/src"), None, Some(home)),
            "src"
        );
    }
}
